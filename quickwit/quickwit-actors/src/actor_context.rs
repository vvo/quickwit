// Copyright (C) 2022 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use quickwit_common::metrics::IntCounter;
use quickwit_common::{KillSwitch, Progress, ProtectedZoneGuard};
use tokio::sync::{oneshot, watch};
use tracing::{debug, error};

use crate::actor_state::AtomicState;
use crate::registry::ActorRegistry;
use crate::scheduler::{Callback, ScheduleEvent, Scheduler};
use crate::spawn_builder::SpawnBuilder;
#[cfg(any(test, feature = "testsuite"))]
use crate::Universe;
use crate::{
    Actor, ActorExitStatus, ActorState, AskError, Command, Handler, Mailbox, SendError,
    SpawnContext, TrySendError,
};

// TODO hide all of this public stuff
pub struct ActorContext<A: Actor> {
    inner: Arc<ActorContextInner<A>>,
}

impl<A: Actor> Clone for ActorContext<A> {
    fn clone(&self) -> Self {
        ActorContext {
            inner: self.inner.clone(),
        }
    }
}

impl<A: Actor> Deref for ActorContext<A> {
    type Target = ActorContextInner<A>;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref()
    }
}

pub struct ActorContextInner<A: Actor> {
    self_mailbox: Mailbox<A>,
    progress: Progress,
    kill_switch: KillSwitch,
    scheduler_mailbox: Mailbox<Scheduler>,
    registry: ActorRegistry,
    actor_state: AtomicState,
    backpressure_micros_counter_opt: Option<IntCounter>,
    observable_state_tx: watch::Sender<A::ObservableState>,
}

impl<A: Actor> ActorContext<A> {
    pub(crate) fn new(
        self_mailbox: Mailbox<A>,
        kill_switch: KillSwitch,
        scheduler_mailbox: Mailbox<Scheduler>,
        registry: ActorRegistry,
        observable_state_tx: watch::Sender<A::ObservableState>,
        backpressure_micros_counter_opt: Option<IntCounter>,
    ) -> Self {
        ActorContext {
            inner: ActorContextInner {
                self_mailbox,
                progress: Progress::default(),
                kill_switch,
                scheduler_mailbox,
                registry,
                actor_state: AtomicState::default(),
                observable_state_tx,
                backpressure_micros_counter_opt,
            }
            .into(),
        }
    }

    #[cfg(any(test, feature = "testsuite"))]
    pub fn for_test(
        universe: &Universe,
        actor_mailbox: Mailbox<A>,
        observable_state_tx: watch::Sender<A::ObservableState>,
    ) -> Self {
        Self::new(
            actor_mailbox,
            universe.kill_switch.clone(),
            universe.scheduler_mailbox.clone(),
            universe.registry.clone(),
            observable_state_tx,
            None,
        )
    }

    pub fn mailbox(&self) -> &Mailbox<A> {
        &self.self_mailbox
    }

    pub(crate) fn registry(&self) -> &ActorRegistry {
        &self.registry
    }

    pub fn actor_instance_id(&self) -> &str {
        self.mailbox().actor_instance_id()
    }

    /// This function returns a guard that prevents any supervisor from identifying the
    /// actor as dead.
    /// The protection ends when the `ProtectZoneGuard` is dropped.
    ///
    /// In an ideal world, you should never need to call this function.
    /// It is only useful in some corner cases, like calling a long blocking
    /// from an external library that you trust.
    pub fn protect_zone(&self) -> ProtectedZoneGuard {
        self.progress.protect_zone()
    }

    /// Executes a future in a protected zone.
    pub async fn protect_future<Fut, T>(&self, future: Fut) -> T
    where Fut: Future<Output = T> {
        let _guard = self.protect_zone();
        future.await
    }

    /// Cooperatively yields, while keeping the actor protected.
    pub async fn yield_now(&self) {
        self.protect_future(tokio::task::yield_now()).await;
    }

    /// Gets a copy of the actor kill switch.
    /// This should rarely be used.
    ///
    /// For instance, when quitting from the process_message function, prefer simply
    /// returning `Error(ActorExitStatus::Failure(..))`
    pub fn kill_switch(&self) -> &KillSwitch {
        &self.kill_switch
    }

    #[must_use]
    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    pub fn spawn_actor<SpawnedActor: Actor>(&self) -> SpawnBuilder<SpawnedActor> {
        SpawnBuilder::new(
            self.scheduler_mailbox.clone(),
            self.kill_switch.child(),
            self.registry.clone(),
        )
    }

    /// Returns a spawn context associated with the current universe.
    pub fn spawn_ctx(&self) -> &SpawnContext {
        &SpawnContext
    }

    /// Records some progress.
    /// This function is only useful when implementing actors that may take more than
    /// `HEARTBEAT` to process a single message.
    /// In that case, you can call this function in the middle of the process_message method
    /// to prevent the actor from being identified as blocked or dead.
    pub fn record_progress(&self) {
        self.progress.record_progress();
    }

    pub(crate) fn state(&self) -> ActorState {
        self.actor_state.get_state()
    }

    pub(crate) fn process(&self) {
        self.actor_state.process();
    }

    pub(crate) fn idle(&self) {
        self.actor_state.idle();
    }

    pub(crate) fn pause(&self) {
        self.actor_state.pause();
    }

    pub(crate) fn resume(&self) {
        self.actor_state.resume();
    }

    pub(crate) fn observe(&self, actor: &mut A) -> A::ObservableState {
        let obs_state = actor.observable_state();
        let _ = self.observable_state_tx.send(obs_state.clone());
        obs_state
    }

    pub(crate) fn exit(&self, exit_status: &ActorExitStatus) {
        self.actor_state.exit(exit_status.is_success());
        if should_activate_kill_switch(exit_status) {
            error!(actor=%self.actor_instance_id(), exit_status=?exit_status, "exit activating-kill-switch");
            self.kill_switch().kill();
        }
    }
}

impl<A: Actor> ActorContext<A> {
    /// Posts a message in an actor's mailbox.
    ///
    /// This method does not wait for the message to be handled by the
    /// target actor. However, it returns a oneshot receiver that the caller
    /// that makes it possible to `.await` it.
    /// If the reply is important, chances are the `.ask(...)` method is
    /// more indicated.
    ///
    /// Droppping the receiver channel will not cancel the
    /// processing of the messsage. It is a very common usage.
    /// In fact most actors are expected to send message in a
    /// fire-and-forget fashion.
    ///
    /// Regular messages (as opposed to commands) are queued and guaranteed
    /// to be processed in FIFO order.
    ///
    /// This method hides logic to prevent an actor from being identified
    /// as frozen if the destination actor channel is saturated, and we
    /// are simply experiencing back pressure.
    pub async fn send_message<DestActor: Actor, M>(
        &self,
        mailbox: &Mailbox<DestActor>,
        msg: M,
    ) -> Result<oneshot::Receiver<DestActor::Reply>, SendError>
    where
        DestActor: Handler<M>,
        M: 'static + Send + Sync + fmt::Debug,
    {
        let _guard = self.protect_zone();
        debug!(from=%self.self_mailbox.actor_instance_id(), send=%mailbox.actor_instance_id(), msg=?msg);
        mailbox
            .send_message_with_backpressure_counter(
                msg,
                self.backpressure_micros_counter_opt.as_ref(),
            )
            .await
    }

    pub async fn ask<DestActor: Actor, M, T>(
        &self,
        mailbox: &Mailbox<DestActor>,
        msg: M,
    ) -> Result<T, AskError<Infallible>>
    where
        DestActor: Handler<M, Reply = T>,
        M: 'static + Send + Sync + fmt::Debug,
    {
        let _guard = self.protect_zone();
        debug!(from=%self.self_mailbox.actor_instance_id(), send=%mailbox.actor_instance_id(), msg=?msg, "ask");
        mailbox
            .ask_with_backpressure_counter(msg, self.backpressure_micros_counter_opt.as_ref())
            .await
    }

    /// Similar to `send_message`, except this method
    /// waits asynchronously for the actor reply.
    pub async fn ask_for_res<DestActor: Actor, M, T, E: fmt::Debug>(
        &self,
        mailbox: &Mailbox<DestActor>,
        msg: M,
    ) -> Result<T, AskError<E>>
    where
        DestActor: Handler<M, Reply = Result<T, E>>,
        M: 'static + Send + Sync + fmt::Debug,
    {
        let _guard = self.protect_zone();
        debug!(from=%self.self_mailbox.actor_instance_id(), send=%mailbox.actor_instance_id(), msg=?msg, "ask");
        mailbox.ask_for_res(msg).await
    }

    /// Send the Success message to terminate the destination actor with the Success exit status.
    ///
    /// The message is queued like any regular message, so that pending messages will be processed
    /// first.
    pub async fn send_exit_with_success<Dest: Actor>(
        &self,
        mailbox: &Mailbox<Dest>,
    ) -> Result<(), SendError> {
        let _guard = self.protect_zone();
        debug!(from=%self.self_mailbox.actor_instance_id(), to=%mailbox.actor_instance_id(), "success");
        mailbox.send_message(Command::ExitWithSuccess).await?;
        Ok(())
    }

    /// Sends a message to an actor's own mailbox.
    ///
    /// Warning: This method is dangerous as it can very easily
    /// cause a deadlock.
    pub async fn send_self_message<M>(
        &self,
        msg: M,
    ) -> Result<oneshot::Receiver<A::Reply>, SendError>
    where
        A: Handler<M>,
        M: 'static + Sync + Send + fmt::Debug,
    {
        debug!(self=%self.self_mailbox.actor_instance_id(), msg=?msg, "self_send");
        self.self_mailbox.send_message(msg).await
    }

    /// Attempts to send a message to itself.
    ///
    /// Warning: This method will always fail if
    /// an actor has a capacity of 0.
    pub fn try_send_self_message<M>(
        &self,
        msg: M,
    ) -> Result<oneshot::Receiver<A::Reply>, TrySendError<M>>
    where
        A: Handler<M>,
        M: 'static + Sync + Send + fmt::Debug,
    {
        self.self_mailbox.try_send_message(msg)
    }

    pub async fn schedule_self_msg<M>(&self, after_duration: Duration, message: M)
    where
        A: Handler<M>,
        M: Sync + Send + std::fmt::Debug + 'static,
    {
        let self_mailbox = self.inner.self_mailbox.clone();
        let callback = Callback(Box::pin(async move {
            let _ = self_mailbox.send_message_with_high_priority(message);
        }));
        let scheduler_msg = ScheduleEvent {
            timeout: after_duration,
            callback,
        };
        let _ = self
            .send_message(&self.inner.scheduler_mailbox, scheduler_msg)
            .await;
    }
}

/// If an actor exits in an unexpected manner, its kill
/// switch will be activated, and all other actors under the same
/// kill switch will be killed.
fn should_activate_kill_switch(exit_status: &ActorExitStatus) -> bool {
    match exit_status {
        ActorExitStatus::DownstreamClosed => true,
        ActorExitStatus::Failure(_) => true,
        ActorExitStatus::Panicked => true,
        ActorExitStatus::Success => false,
        ActorExitStatus::Quit => false,
        ActorExitStatus::Killed => false,
    }
}
