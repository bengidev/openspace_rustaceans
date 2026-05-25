//! Home shell crate.
//!
//! Composes the three first-class modes — terminal, chat, editor — into
//! a single workspace surface. Each mode lives in its own sub-feature
//! crate under `sub-features/` and is wired up here.

use openspace_observability::{
    Notification, NotificationStream, NotificationStreamRecvError, Observability, Severity,
};

pub mod placeholder {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToastPresenterState {
    pub notification: Notification,
    pub show_details: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BannerPresenterState {
    pub notification: Notification,
    pub persistent: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModalPresenterState {
    pub notification: Notification,
    pub actions: Vec<ModalAction>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModalAction {
    Retry,
    Skip,
    Report,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HomeNotificationPresenterState {
    pub toast: Option<ToastPresenterState>,
    pub banner: Option<BannerPresenterState>,
    pub modal: Option<ModalPresenterState>,
    pub error_log_panel: Vec<Notification>,
}

pub struct HomeNotificationConsumer {
    stream: NotificationStream,
    state: HomeNotificationPresenterState,
}

impl HomeNotificationConsumer {
    #[must_use]
    pub fn new(observability: &Observability) -> Self {
        Self {
            stream: observability.subscribe(),
            state: HomeNotificationPresenterState {
                error_log_panel: observability.log().entries(),
                ..HomeNotificationPresenterState::default()
            },
        }
    }

    #[must_use]
    pub const fn state(&self) -> &HomeNotificationPresenterState {
        &self.state
    }

    pub fn apply(&mut self, notification: Notification) {
        self.state.error_log_panel.push(notification.clone());
        apply_notification(&mut self.state, notification);
    }

    pub async fn sync_next(&mut self) -> Result<(), NotificationStreamRecvError> {
        let notification = self.stream.recv().await?;
        self.apply(notification);
        Ok(())
    }
}

#[must_use]
pub fn state_from_log(observability: &Observability) -> HomeNotificationPresenterState {
    let mut state = HomeNotificationPresenterState {
        error_log_panel: observability.log().entries(),
        ..HomeNotificationPresenterState::default()
    };
    for notification in state.error_log_panel.clone() {
        apply_notification(&mut state, notification);
    }
    state
}

fn apply_notification(state: &mut HomeNotificationPresenterState, notification: Notification) {
    match notification.severity {
        Severity::Low => {}
        Severity::Medium => {
            state.toast = Some(ToastPresenterState {
                notification,
                show_details: true,
            });
        }
        Severity::High => {
            state.banner = Some(BannerPresenterState {
                notification,
                persistent: true,
            });
        }
        Severity::Critical => {
            state.modal = Some(ModalPresenterState {
                notification,
                actions: vec![ModalAction::Retry, ModalAction::Skip, ModalAction::Report],
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openspace_observability::Severity;

    #[test]
    fn medium_notifications_create_toast_with_details_affordance() {
        let observability = Observability::default();
        let notification = observability.emit(Severity::Medium, "sync", "details available");

        let state = state_from_log(&observability);

        assert_eq!(
            state.toast.as_ref().map(|toast| &toast.notification),
            Some(&notification)
        );
        assert_eq!(
            state.toast.as_ref().map(|toast| toast.show_details),
            Some(true)
        );
        assert_eq!(state.error_log_panel, [notification]);
    }

    #[test]
    fn high_notifications_create_persistent_banner() {
        let observability = Observability::default();
        let notification = observability.emit(Severity::High, "network", "offline");

        let state = state_from_log(&observability);

        assert_eq!(
            state.banner.as_ref().map(|banner| &banner.notification),
            Some(&notification)
        );
        assert_eq!(
            state.banner.as_ref().map(|banner| banner.persistent),
            Some(true)
        );
        assert_eq!(state.error_log_panel, [notification]);
    }

    #[test]
    fn critical_notifications_create_modal_with_recovery_actions() {
        let observability = Observability::default();
        let notification = observability.emit(Severity::Critical, "runtime", "failed");

        let state = state_from_log(&observability);

        let modal = state.modal.expect("critical event maps to modal state");
        assert_eq!(modal.notification, notification);
        assert_eq!(
            modal.actions,
            [ModalAction::Retry, ModalAction::Skip, ModalAction::Report]
        );
    }

    #[tokio::test]
    async fn consumer_reads_same_stream_as_error_log_panel() {
        let observability = Observability::default();
        let mut consumer = HomeNotificationConsumer::new(&observability);
        let notification = observability.emit(Severity::Medium, "sync", "queued");

        consumer.sync_next().await.expect("notification received");

        assert_eq!(consumer.state().error_log_panel, [notification]);
        assert!(consumer.state().toast.is_some());
    }
}
