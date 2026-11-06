use std::time::Duration;

use regex::Regex;

use crate::api::event_hub::EventHistoryError;
use crate::api::event_hub::WaitTiming;
use crate::api::event_hub::WakeInterest;
use crate::api::schema::{
    ErrorBody, ErrorResponse, EventKind, Method, PaneAgentStatusChangedEvent,
    PaneOutputMatchedEvent, PaneReadState, PaneScrollChangedEvent, PaneScrollInfo, Request,
    Subscription, SubscriptionEventData, SubscriptionEventEnvelope, SubscriptionEventKind,
};
use crate::api::server::{
    dispatch_to_app_with_timeout, APP_RESPONSE_TIMEOUT, CONNECTION_POLL_INTERVAL,
};
use crate::api::{ApiRequestSender, EventHub};

pub(super) fn output_match_read_source(
    source: &crate::api::schema::ReadSource,
) -> crate::api::schema::ReadSource {
    match source {
        crate::api::schema::ReadSource::Recent => crate::api::schema::ReadSource::RecentUnwrapped,
        other => *other,
    }
}

pub(super) fn match_output(
    text: &str,
    matcher: &crate::api::schema::OutputMatch,
    regex: Option<&Regex>,
) -> Option<String> {
    match matcher {
        crate::api::schema::OutputMatch::Substring { value } => text
            .lines()
            .find(|line| line.contains(value))
            .map(|line| line.to_string()),
        crate::api::schema::OutputMatch::Regex { .. } => regex.and_then(|re| {
            text.lines()
                .find(|line| re.is_match(line))
                .map(|line| line.to_string())
        }),
    }
}

pub(super) struct ActiveOutputMatchedSubscription {
    pane_id: String,
    source: crate::api::schema::ReadSource,
    lines: Option<u32>,
    matcher: crate::api::schema::OutputMatch,
    regex: Option<Regex>,
    strip_ansi: bool,
    currently_matching: bool,
    request_prefix: String,
}

pub(super) struct ActiveAgentStatusChangedSubscription {
    pane_id: String,
    status_filter: Option<crate::api::schema::AgentStatus>,
    last_status: Option<crate::api::schema::AgentStatus>,
    last_presentation: Option<PanePresentationSnapshot>,
    last_sequence: u64,
    initial_event: Option<PaneAgentStatusChangedEvent>,
    /// Set when the history held an event about this pane that is not a status change (see
    /// `note_pane_event`); the next pass takes a snapshot at once. Cleared when one has run.
    snapshot_requested: bool,
    request_prefix: String,
}

pub(super) struct ActiveScrollChangedSubscription {
    pane_id: String,
    last_scroll: Option<PaneScrollInfo>,
    request_prefix: String,
}

/// Relays the app's `pane.read_state_changed` hub events for one pane. The app
/// diffs every pane's read state once per loop turn (`App::sync_read_state_events`),
/// so this subscription only filters; it never polls the app.
pub(super) struct ActiveReadStateChangedSubscription {
    pane_id: String,
    last_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PanePresentationSnapshot {
    title: Option<String>,
    display_agent: Option<String>,
    state_labels: std::collections::HashMap<String, String>,
}

impl PanePresentationSnapshot {
    fn from(pane: &crate::api::schema::PaneInfo) -> Self {
        Self {
            title: pane.title.clone(),
            display_agent: pane.display_agent.clone(),
            state_labels: pane.state_labels.clone(),
        }
    }

    fn from_event(
        title: &Option<String>,
        display_agent: &Option<String>,
        state_labels: &std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            title: title.clone(),
            display_agent: display_agent.clone(),
            state_labels: state_labels.clone(),
        }
    }
}

pub(super) struct ActiveEventSubscription {
    event_kind: crate::api::schema::EventKind,
    last_sequence: u64,
}

pub(super) enum ActiveSubscription {
    Event(ActiveEventSubscription),
    OutputMatched(ActiveOutputMatchedSubscription),
    AgentStatusChanged(Box<ActiveAgentStatusChangedSubscription>),
    ScrollChanged(ActiveScrollChangedSubscription),
    ReadStateChanged(ActiveReadStateChangedSubscription),
}

impl ActiveSubscription {
    pub(super) fn new(
        subscription: Subscription,
        request_id: &str,
        index: usize,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
        event_start_sequence: u64,
    ) -> Result<Self, ErrorResponse> {
        let event_subscription = |event_kind| {
            Self::Event(ActiveEventSubscription {
                event_kind,
                last_sequence: event_start_sequence,
            })
        };

        match subscription {
            Subscription::WorkspaceCreated {} => {
                Ok(event_subscription(EventKind::WorkspaceCreated))
            }
            Subscription::WorkspaceUpdated {} => {
                Ok(event_subscription(EventKind::WorkspaceUpdated))
            }
            Subscription::WorkspaceMetadataUpdated {} => {
                Ok(event_subscription(EventKind::WorkspaceMetadataUpdated))
            }
            Subscription::WorkspaceRenamed {} => {
                Ok(event_subscription(EventKind::WorkspaceRenamed))
            }
            Subscription::WorkspaceMoved {} => Ok(event_subscription(EventKind::WorkspaceMoved)),
            Subscription::WorkspaceReordered {} => {
                Ok(event_subscription(EventKind::WorkspaceReordered))
            }
            Subscription::WorkspaceClosed {} => Ok(event_subscription(EventKind::WorkspaceClosed)),
            Subscription::WorkspaceFocused {} => {
                Ok(event_subscription(EventKind::WorkspaceFocused))
            }
            Subscription::WorktreeCreated {} => Ok(event_subscription(EventKind::WorktreeCreated)),
            Subscription::WorktreeOpened {} => Ok(event_subscription(EventKind::WorktreeOpened)),
            Subscription::WorktreeRemoved {} => Ok(event_subscription(EventKind::WorktreeRemoved)),
            Subscription::TabCreated {} => Ok(event_subscription(EventKind::TabCreated)),
            Subscription::TabClosed {} => Ok(event_subscription(EventKind::TabClosed)),
            Subscription::TabFocused {} => Ok(event_subscription(EventKind::TabFocused)),
            Subscription::TabRenamed {} => Ok(event_subscription(EventKind::TabRenamed)),
            Subscription::TabMoved {} => Ok(event_subscription(EventKind::TabMoved)),
            Subscription::PaneCreated {} => Ok(event_subscription(EventKind::PaneCreated)),
            Subscription::PaneClosed {} => Ok(event_subscription(EventKind::PaneClosed)),
            Subscription::PaneUpdated {} => Ok(event_subscription(EventKind::PaneUpdated)),
            Subscription::PaneFocused {} => Ok(event_subscription(EventKind::PaneFocused)),
            Subscription::PaneMoved {} => Ok(event_subscription(EventKind::PaneMoved)),
            Subscription::PaneExited {} => Ok(event_subscription(EventKind::PaneExited)),
            Subscription::PaneAgentDetected {} => {
                Ok(event_subscription(EventKind::PaneAgentDetected))
            }
            Subscription::LayoutUpdated {} => Ok(event_subscription(EventKind::LayoutUpdated)),
            Subscription::PaneOutputMatched {
                pane_id,
                source,
                lines,
                r#match,
                strip_ansi,
            } => {
                let regex = match &r#match {
                    crate::api::schema::OutputMatch::Regex { value } => match Regex::new(value) {
                        Ok(regex) => Some(regex),
                        Err(err) => {
                            return Err(ErrorResponse {
                                id: request_id.to_string(),
                                error: ErrorBody {
                                    code: "invalid_regex".into(),
                                    message: err.to_string(),
                                },
                            });
                        }
                    },
                    crate::api::schema::OutputMatch::Substring { .. } => None,
                };

                let probe = pane_read(
                    format!("{request_id}:sub:{index}:probe"),
                    &pane_id,
                    source,
                    lines,
                    strip_ansi,
                    api_tx,
                );
                probe?;

                Ok(Self::OutputMatched(ActiveOutputMatchedSubscription {
                    pane_id,
                    source,
                    lines,
                    matcher: r#match,
                    regex,
                    strip_ansi,
                    currently_matching: false,
                    request_prefix: format!("{request_id}:sub:{index}"),
                }))
            }
            Subscription::PaneAgentStatusChanged {
                pane_id,
                agent_status,
            } => {
                let last_sequence = event_hub.current_sequence();
                let probe = pane_get(format!("{request_id}:sub:{index}:probe"), &pane_id, api_tx)?;
                let last_status = probe.agent_status;
                let last_presentation = PanePresentationSnapshot::from(&probe);
                let initial_event = agent_status
                    .is_some_and(|wanted| wanted == probe.agent_status)
                    .then_some(PaneAgentStatusChangedEvent {
                        pane_id: probe.pane_id.clone(),
                        workspace_id: probe.workspace_id,
                        agent_status: probe.agent_status,
                        agent: probe.agent,
                        title: probe.title,
                        display_agent: probe.display_agent,
                        state_labels: probe.state_labels,
                    });

                Ok(Self::AgentStatusChanged(Box::new(
                    ActiveAgentStatusChangedSubscription {
                        pane_id: probe.pane_id,
                        status_filter: agent_status,
                        last_status: Some(last_status),
                        last_presentation: Some(last_presentation),
                        last_sequence,
                        initial_event,
                        snapshot_requested: false,
                        request_prefix: format!("{request_id}:sub:{index}"),
                    },
                )))
            }
            Subscription::PaneScrollChanged { pane_id } => {
                let probe = pane_get(format!("{request_id}:sub:{index}:probe"), &pane_id, api_tx)?;

                Ok(Self::ScrollChanged(ActiveScrollChangedSubscription {
                    pane_id: probe.pane_id,
                    last_scroll: probe.scroll,
                    request_prefix: format!("{request_id}:sub:{index}"),
                }))
            }
            Subscription::PaneReadStateChanged { pane_id } => {
                // The probe resolves the id the way `pane.read_state` does and fails
                // with `pane_not_found` for an unknown pane. It is not a baseline:
                // events are taken from the start of the request, so a change that
                // lands while the subscription is being set up is still delivered.
                let probe =
                    pane_read_state(format!("{request_id}:sub:{index}:probe"), &pane_id, api_tx)?;

                Ok(Self::ReadStateChanged(ActiveReadStateChangedSubscription {
                    pane_id: probe.pane_id,
                    last_sequence: event_start_sequence,
                }))
            }
        }
    }

    pub(super) fn poll(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Option<serde_json::Value> {
        match self {
            Self::Event(subscription) => subscription.poll(event_hub),
            Self::OutputMatched(subscription) => {
                serde_json::to_value(subscription.poll(api_tx)?).ok()
            }
            Self::AgentStatusChanged(subscription) => {
                serde_json::to_value(subscription.poll(api_tx, event_hub)?).ok()
            }
            Self::ScrollChanged(subscription) => {
                serde_json::to_value(subscription.poll(api_tx)?).ok()
            }
            Self::ReadStateChanged(subscription) => {
                serde_json::to_value(subscription.poll(event_hub)?).ok()
            }
        }
    }

    /// The events that can change what this subscription does, so the hub wakes the connection
    /// thread for those and no others (see `WakeInterest`). It sits beside the code that consumes
    /// the events and must stay in step with it: `wake_interests_cover_every_event_a_subscription_reacts_to`
    /// runs an event of every kind through each subscription and fails when one reacts to an event
    /// its interests do not match.
    pub(super) fn wake_interests(&self) -> Vec<WakeInterest> {
        match self {
            Self::Event(subscription) => vec![WakeInterest::kind(subscription.event_kind)],
            // Terminal output and scroll position are sampled on a timer and never read from the
            // history: no event changes what these do.
            Self::OutputMatched(_) | Self::ScrollChanged(_) => Vec::new(),
            // Every event about its pane (status, title, close, move, read flag, ...) and, through
            // the hub, every event that may remove or renumber its pane without naming it.
            Self::AgentStatusChanged(subscription) => {
                vec![WakeInterest::pane(subscription.pane_id.clone())]
            }
            // Only the read-state events of its own pane.
            Self::ReadStateChanged(subscription) => vec![WakeInterest::pane_kind(
                subscription.pane_id.clone(),
                EventKind::PaneReadStateChanged,
            )],
        }
    }

    /// How often this subscription must sample live state while no event wakes it, or `None` when
    /// the retained event history is all it reads (such a subscription can sleep until the hub
    /// wakes it). Output and scroll subscriptions keep their original 100 ms cadence: terminal
    /// output and scroll position are never published as events.
    pub(super) fn sample_interval(&self, timing: &WaitTiming) -> Option<Duration> {
        match self {
            Self::Event(_) | Self::ReadStateChanged(_) => None,
            Self::AgentStatusChanged(_) => Some(timing.agent_status_snapshot),
            Self::OutputMatched(_) | Self::ScrollChanged(_) => Some(CONNECTION_POLL_INTERVAL),
        }
    }

    /// `sample_due` says whether this pass may sample live state (see `sample_interval`); a pass
    /// that an event woke early reads the history only.
    pub(super) fn poll_for_wait(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
        sample_due: bool,
    ) -> Result<Option<serde_json::Value>, ErrorResponse> {
        match self {
            Self::AgentStatusChanged(subscription) => Ok(subscription
                .poll_result(api_tx, event_hub, sample_due)?
                .and_then(|event| serde_json::to_value(event).ok())),
            Self::OutputMatched(_) | Self::ScrollChanged(_) if !sample_due => Ok(None),
            _ => Ok(self.poll(api_tx, event_hub)),
        }
    }

    pub(super) fn poll_batch(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
        sample_due: bool,
    ) -> Result<Vec<serde_json::Value>, ErrorBody> {
        match self {
            Self::Event(subscription) => {
                let events = subscription_events_after(event_hub, subscription.last_sequence)?;
                let mut matching = Vec::new();
                for (sequence, event) in events {
                    subscription.last_sequence = sequence;
                    if event.event == subscription.event_kind {
                        matching.push(serde_json::to_value(event).map_err(event_encoding_error)?);
                    }
                }
                Ok(matching)
            }
            Self::AgentStatusChanged(subscription) => {
                let events = subscription_events_after(event_hub, subscription.last_sequence)?;
                let mut matching = Vec::new();
                for (sequence, event) in events {
                    subscription.last_sequence = sequence;
                    if let Some(event) = subscription.event_from_history(event) {
                        matching.push(serde_json::to_value(event).map_err(event_encoding_error)?);
                    }
                }
                if matching.is_empty() && (sample_due || subscription.snapshot_requested) {
                    if let Some(event) =
                        subscription.poll_snapshot(api_tx, event_hub).ok().flatten()
                    {
                        matching.push(serde_json::to_value(event).map_err(event_encoding_error)?);
                    }
                }
                Ok(matching)
            }
            Self::ReadStateChanged(subscription) => {
                let events = subscription_events_after(event_hub, subscription.last_sequence)?;
                let mut matching = Vec::new();
                for (sequence, event) in events {
                    subscription.last_sequence = sequence;
                    if let Some(event) = subscription.event_from_history(event) {
                        matching.push(serde_json::to_value(event).map_err(event_encoding_error)?);
                    }
                }
                Ok(matching)
            }
            // These subscriptions sample current state, not retained event history.
            // Keep their existing cadence even when a lifecycle batch was nonempty: they
            // sample when `sample_due`, not every time an unrelated event wakes the connection.
            Self::OutputMatched(_) | Self::ScrollChanged(_) => {
                if sample_due {
                    Ok(self.poll(api_tx, event_hub).into_iter().collect())
                } else {
                    Ok(Vec::new())
                }
            }
        }
    }
}

fn subscription_events_after(
    event_hub: &EventHub,
    sequence: u64,
) -> Result<Vec<(u64, crate::api::schema::EventEnvelope)>, ErrorBody> {
    event_hub.events_after_checked(sequence).map_err(|error| match error {
        EventHistoryError::Lost => ErrorBody {
            code: "events_lost".into(),
            message: "event subscription fell behind retained history; resubscribe and resync with session.snapshot".into(),
        },
        EventHistoryError::Unavailable => ErrorBody {
            code: "server_unavailable".into(),
            message: "event history is unavailable".into(),
        },
    })
}

fn event_encoding_error(error: serde_json::Error) -> ErrorBody {
    ErrorBody {
        code: "internal_error".into(),
        message: format!("failed to encode subscription event: {error}"),
    }
}

impl ActiveEventSubscription {
    fn poll(&mut self, event_hub: &EventHub) -> Option<serde_json::Value> {
        for (sequence, event) in event_hub.events_after(self.last_sequence) {
            self.last_sequence = sequence;
            if event.event == self.event_kind {
                return serde_json::to_value(event).ok();
            }
        }
        None
    }
}

impl ActiveOutputMatchedSubscription {
    fn poll(&mut self, api_tx: &ApiRequestSender) -> Option<SubscriptionEventEnvelope> {
        let read = pane_read(
            format!("{}:read", self.request_prefix),
            &self.pane_id,
            output_match_read_source(&self.source),
            self.lines,
            self.strip_ansi,
            api_tx,
        )
        .ok()?;

        let matched_line = match_output(&read.text, &self.matcher, self.regex.as_ref());
        match matched_line {
            Some(matched_line) => {
                if self.currently_matching {
                    return None;
                }
                self.currently_matching = true;
                Some(SubscriptionEventEnvelope {
                    event: SubscriptionEventKind::PaneOutputMatched,
                    data: SubscriptionEventData::PaneOutputMatched(PaneOutputMatchedEvent {
                        pane_id: read.pane_id.clone(),
                        matched_line,
                        read,
                    }),
                })
            }
            None => {
                self.currently_matching = false;
                None
            }
        }
    }
}

impl ActiveAgentStatusChangedSubscription {
    fn poll(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Option<SubscriptionEventEnvelope> {
        self.poll_result(api_tx, event_hub, true).ok().flatten()
    }

    /// `snapshot` false reads the retained history only; true also asks the app for the pane
    /// when the history had nothing for this subscription.
    fn poll_result(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
        snapshot: bool,
    ) -> Result<Option<SubscriptionEventEnvelope>, ErrorResponse> {
        for (sequence, event) in event_hub.events_after(self.last_sequence) {
            self.last_sequence = sequence;
            if let Some(event) = self.event_from_history(event) {
                return Ok(Some(event));
            }
        }

        if snapshot || self.snapshot_requested {
            self.poll_snapshot(api_tx, event_hub)
        } else {
            Ok(None)
        }
    }

    /// An event about this pane that is not a `pane.agent_status_changed` (the pane closed,
    /// exited or moved, an agent was detected or released, the pane was updated, or its read flag
    /// changed) can change what `pane.get` answers, or end it with `pane_not_found`. The status
    /// events themselves are consumed from the history directly, so a title, display-agent or
    /// state-label change needs no snapshot; these need one, and at once rather than at the next
    /// interval.
    ///
    /// A read-flag change is the only record of a done pane that was viewed (or got the
    /// `auto_read` token late): the projected status goes from done to idle with no status event.
    /// A tab or workspace that closes, or a tab that moves to another workspace, emits no
    /// `pane.closed` for the panes inside it and renames their ids; those events carry no pane
    /// list, so any of them requests the snapshot (they are rare).
    fn note_pane_event(&mut self, event: &crate::api::schema::EventEnvelope) {
        use crate::api::schema::EventData;
        let concerns_pane = match &event.data {
            EventData::PaneClosed { pane_id, .. }
            | EventData::PaneExited { pane_id, .. }
            | EventData::PaneAgentDetected { pane_id, .. } => *pane_id == self.pane_id,
            EventData::PaneMoved {
                previous_pane_id, ..
            } => *previous_pane_id == self.pane_id,
            EventData::PaneUpdated { pane } => pane.pane_id == self.pane_id,
            EventData::PaneReadStateChanged { read_state } => read_state.pane_id == self.pane_id,
            EventData::TabClosed { .. }
            | EventData::WorkspaceClosed { .. }
            | EventData::TabMoved { .. } => true,
            _ => false,
        };
        if concerns_pane {
            self.snapshot_requested = true;
        }
    }

    fn event_from_history(
        &mut self,
        event: crate::api::schema::EventEnvelope,
    ) -> Option<SubscriptionEventEnvelope> {
        self.note_pane_event(&event);
        if event.event != EventKind::PaneAgentStatusChanged {
            return None;
        }
        let crate::api::schema::EventData::PaneAgentStatusChanged {
            pane_id,
            workspace_id,
            agent_status,
            agent,
            title,
            display_agent,
            state_labels,
        } = event.data
        else {
            return None;
        };
        if pane_id != self.pane_id {
            return None;
        }
        self.last_status = Some(agent_status);
        self.last_presentation = Some(PanePresentationSnapshot::from_event(
            &title,
            &display_agent,
            &state_labels,
        ));
        self.initial_event = None;
        if self
            .status_filter
            .is_some_and(|wanted| wanted != agent_status)
        {
            return None;
        }

        Some(SubscriptionEventEnvelope {
            event: SubscriptionEventKind::PaneAgentStatusChanged,
            data: SubscriptionEventData::PaneAgentStatusChanged(PaneAgentStatusChangedEvent {
                pane_id,
                workspace_id,
                agent_status,
                agent,
                title,
                display_agent,
                state_labels,
            }),
        })
    }

    fn poll_snapshot(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Result<Option<SubscriptionEventEnvelope>, ErrorResponse> {
        if event_hub.current_sequence() != self.last_sequence {
            return Ok(None);
        } else if let Some(event) = self.initial_event.take() {
            return Ok(Some(SubscriptionEventEnvelope {
                event: SubscriptionEventKind::PaneAgentStatusChanged,
                data: SubscriptionEventData::PaneAgentStatusChanged(event),
            }));
        }

        let before_snapshot_sequence = self.last_sequence;
        let pane = pane_get(
            format!("{}:pane", self.request_prefix),
            &self.pane_id,
            api_tx,
        );
        let after_snapshot_sequence = event_hub.current_sequence();
        if after_snapshot_sequence != before_snapshot_sequence {
            return Ok(None);
        }
        // A snapshot ran against an unchanged history: a pending request is served.
        self.snapshot_requested = false;
        let pane = pane?;

        let event = self.event_from_snapshot(pane);
        if event.is_some() {
            self.last_sequence = after_snapshot_sequence;
        }
        Ok(event)
    }

    fn event_from_snapshot(
        &mut self,
        pane: crate::api::schema::PaneInfo,
    ) -> Option<SubscriptionEventEnvelope> {
        let current_status = pane.agent_status;
        let current_presentation = PanePresentationSnapshot::from(&pane);
        let previous_status = self.last_status.replace(current_status);
        let previous_presentation = self.last_presentation.replace(current_presentation.clone());
        let presentation_changed = previous_presentation
            .as_ref()
            .is_some_and(|previous| previous != &current_presentation);
        let status_changed = previous_status.is_some_and(|previous| previous != current_status);
        if !(status_changed || presentation_changed) {
            return None;
        }
        if self
            .status_filter
            .is_some_and(|wanted| wanted != current_status)
        {
            return None;
        }

        Some(SubscriptionEventEnvelope {
            event: SubscriptionEventKind::PaneAgentStatusChanged,
            data: SubscriptionEventData::PaneAgentStatusChanged(PaneAgentStatusChangedEvent {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                agent_status: current_status,
                agent: pane.agent,
                title: pane.title,
                display_agent: pane.display_agent,
                state_labels: pane.state_labels,
            }),
        })
    }
}

impl ActiveReadStateChangedSubscription {
    fn poll(&mut self, event_hub: &EventHub) -> Option<SubscriptionEventEnvelope> {
        for (sequence, event) in event_hub.events_after(self.last_sequence) {
            self.last_sequence = sequence;
            if let Some(event) = self.event_from_history(event) {
                return Some(event);
            }
        }
        None
    }

    /// The subscription's own events, taken from the retained history: this pane's read
    /// state changes, nothing else.
    fn event_from_history(
        &self,
        event: crate::api::schema::EventEnvelope,
    ) -> Option<SubscriptionEventEnvelope> {
        let crate::api::schema::EventData::PaneReadStateChanged { read_state } = event.data else {
            return None;
        };
        if read_state.pane_id != self.pane_id {
            return None;
        }
        Some(SubscriptionEventEnvelope {
            event: SubscriptionEventKind::PaneReadStateChanged,
            data: SubscriptionEventData::PaneReadStateChanged(read_state),
        })
    }
}

impl ActiveScrollChangedSubscription {
    fn poll(&mut self, api_tx: &ApiRequestSender) -> Option<SubscriptionEventEnvelope> {
        let pane = pane_get(
            format!("{}:pane", self.request_prefix),
            &self.pane_id,
            api_tx,
        )
        .ok()?;
        self.event_from_snapshot(pane)
    }

    fn event_from_snapshot(
        &mut self,
        pane: crate::api::schema::PaneInfo,
    ) -> Option<SubscriptionEventEnvelope> {
        let scroll = pane.scroll;
        if self.last_scroll == scroll {
            return None;
        }
        self.last_scroll = scroll;
        let scroll = scroll?;

        Some(SubscriptionEventEnvelope {
            event: SubscriptionEventKind::ScrollChanged,
            data: SubscriptionEventData::ScrollChanged(PaneScrollChangedEvent {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                scroll,
            }),
        })
    }
}

fn pane_read(
    request_id: String,
    pane_id: &str,
    source: crate::api::schema::ReadSource,
    lines: Option<u32>,
    strip_ansi: bool,
    api_tx: &ApiRequestSender,
) -> Result<crate::api::schema::PaneReadResult, ErrorResponse> {
    let response = dispatch_to_app_with_timeout(
        Request {
            id: request_id.clone(),
            method: Method::PaneRead(crate::api::schema::PaneReadParams {
                pane_id: pane_id.to_string(),
                source,
                lines,
                format: crate::api::schema::ReadFormat::Text,
                strip_ansi,
                intent: crate::api::schema::ReadIntent::Passive,
            }),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    let value: serde_json::Value = serde_json::from_str(&response).map_err(|_| ErrorResponse {
        id: request_id.clone(),
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane read response".into(),
        },
    })?;
    if value.get("error").is_some() {
        return serde_json::from_value(value).map_err(|_| ErrorResponse {
            id: request_id,
            error: ErrorBody {
                code: "internal_error".into(),
                message: "failed to decode pane read error".into(),
            },
        });
    }
    serde_json::from_value(value["result"]["read"].clone()).map_err(|_| ErrorResponse {
        id: request_id,
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane read result".into(),
        },
    })
}

fn pane_read_state(
    request_id: String,
    pane_id: &str,
    api_tx: &ApiRequestSender,
) -> Result<PaneReadState, ErrorResponse> {
    let response = dispatch_to_app_with_timeout(
        Request {
            id: request_id.clone(),
            method: Method::PaneReadState(crate::api::schema::PaneTarget {
                pane_id: pane_id.to_string(),
            }),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    let value: serde_json::Value = serde_json::from_str(&response).map_err(|_| ErrorResponse {
        id: request_id.clone(),
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane read state response".into(),
        },
    })?;
    if value.get("error").is_some() {
        return Err(
            serde_json::from_value::<ErrorResponse>(value).map_err(|_| ErrorResponse {
                id: request_id,
                error: ErrorBody {
                    code: "internal_error".into(),
                    message: "failed to decode pane read state error".into(),
                },
            })?,
        );
    }
    serde_json::from_value(value["result"]["read_state"].clone()).map_err(|_| ErrorResponse {
        id: request_id,
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane read state result".into(),
        },
    })
}

fn pane_get(
    request_id: String,
    pane_id: &str,
    api_tx: &ApiRequestSender,
) -> Result<crate::api::schema::PaneInfo, ErrorResponse> {
    let response = dispatch_to_app_with_timeout(
        Request {
            id: request_id.clone(),
            method: Method::PaneGet(crate::api::schema::PaneTarget {
                pane_id: pane_id.to_string(),
            }),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    let value: serde_json::Value = serde_json::from_str(&response).map_err(|_| ErrorResponse {
        id: request_id.clone(),
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane get response".into(),
        },
    })?;
    if value.get("error").is_some() {
        let response =
            serde_json::from_value::<ErrorResponse>(value).map_err(|_| ErrorResponse {
                id: request_id,
                error: ErrorBody {
                    code: "internal_error".into(),
                    message: "failed to decode pane get error".into(),
                },
            })?;
        return Err(response);
    }
    serde_json::from_value(value["result"]["pane"].clone()).map_err(|_| ErrorResponse {
        id: request_id,
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane get result".into(),
        },
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::api::schema::{AgentStatus, EventData, EventEnvelope, EventKind, PaneInfo};

    fn presentation_event(title: Option<&str>) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::PaneAgentStatusChanged,
            data: EventData::PaneAgentStatusChanged {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: title.map(str::to_string),
                display_agent: None,
                state_labels: HashMap::new(),
            },
        }
    }

    fn workspace_focused_event(workspace_id: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::WorkspaceFocused,
            data: EventData::WorkspaceFocused {
                workspace_id: workspace_id.into(),
            },
        }
    }

    fn pane_info_with_scroll(scroll: Option<PaneScrollInfo>) -> PaneInfo {
        PaneInfo {
            pane_id: "pane_1".into(),
            terminal_id: "terminal_1".into(),
            workspace_id: "workspace_1".into(),
            tab_id: "tab_1".into(),
            focused: true,
            cwd: None,
            foreground_cwd: None,
            restore_error: None,
            label: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status: AgentStatus::Unknown,
            state_labels: HashMap::new(),
            tokens: HashMap::new(),
            agent_session: None,
            scroll,
            revision: 0,
        }
    }

    #[test]
    fn lifecycle_subscription_skips_history_but_keeps_setup_window_events() {
        let event_hub = EventHub::default();
        event_hub.push(workspace_focused_event("before_subscription"));
        let event_start_sequence = event_hub.current_sequence();
        event_hub.push(workspace_focused_event("during_setup"));

        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscription = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "test",
            0,
            &api_tx,
            &event_hub,
            event_start_sequence,
        )
        .expect("workspace focus subscription");

        let setup_event = subscription
            .poll(&api_tx, &event_hub)
            .expect("setup-window event");
        assert_eq!(setup_event["data"]["workspace_id"], "during_setup");
        assert!(subscription.poll(&api_tx, &event_hub).is_none());

        event_hub.push(workspace_focused_event("after_setup"));
        let live_event = subscription.poll(&api_tx, &event_hub).expect("live event");
        assert_eq!(live_event["data"]["workspace_id"], "after_setup");
    }

    #[test]
    fn workspace_metadata_subscription_uses_dedicated_event_kind() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscription = ActiveSubscription::new(
            Subscription::WorkspaceMetadataUpdated {},
            "test",
            0,
            &api_tx,
            &event_hub,
            event_hub.current_sequence(),
        )
        .expect("workspace metadata subscription");

        assert!(matches!(
            subscription,
            ActiveSubscription::Event(ActiveEventSubscription {
                event_kind: EventKind::WorkspaceMetadataUpdated,
                ..
            })
        ));
    }

    #[test]
    fn lifecycle_batch_drains_in_order_and_advances_past_unmatched_events() {
        let event_hub = EventHub::default();
        event_hub.push(workspace_focused_event("old"));
        let start = event_hub.current_sequence();
        event_hub.push(workspace_focused_event("setup"));
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscription = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "batch",
            0,
            &api_tx,
            &event_hub,
            start,
        )
        .unwrap();
        event_hub.push(presentation_event(None));
        event_hub.push(workspace_focused_event("live"));
        let events = subscription.poll_batch(&api_tx, &event_hub, true).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["data"]["workspace_id"], "setup");
        assert_eq!(events[1]["data"]["workspace_id"], "live");
        assert!(subscription
            .poll_batch(&api_tx, &event_hub, true)
            .unwrap()
            .is_empty());
        let ActiveSubscription::Event(subscription) = subscription else {
            panic!("expected lifecycle subscription");
        };
        assert_eq!(subscription.last_sequence, event_hub.current_sequence());
    }

    #[test]
    fn read_state_batch_drains_every_change_of_its_pane_in_order() {
        let read_state_event = |pane_id: &str, seen: bool| EventEnvelope {
            event: EventKind::PaneReadStateChanged,
            data: EventData::PaneReadStateChanged {
                read_state: crate::api::schema::PaneReadState {
                    pane_id: pane_id.into(),
                    workspace_id: "workspace_1".into(),
                    seen,
                    agent_status: AgentStatus::Blocked,
                },
            },
        };
        let event_hub = EventHub::default();
        let mut subscription =
            ActiveSubscription::ReadStateChanged(ActiveReadStateChangedSubscription {
                pane_id: "pane_1".into(),
                last_sequence: event_hub.current_sequence(),
            });
        event_hub.push(read_state_event("pane_1", false));
        event_hub.push(read_state_event("pane_2", true));
        event_hub.push(presentation_event(None));
        event_hub.push(read_state_event("pane_1", true));
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();

        // One batch carries both of the pane's changes, in order; the other pane's change
        // and the unrelated event are skipped, so a client sees a block read after it
        // was first reported unread.
        let events = subscription.poll_batch(&api_tx, &event_hub, true).unwrap();
        let seen = events
            .iter()
            .map(|event| event["data"]["seen"].as_bool().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(seen, vec![false, true]);
        assert!(events
            .iter()
            .all(|event| event["event"] == "pane.read_state_changed"));
        assert!(subscription
            .poll_batch(&api_tx, &event_hub, true)
            .unwrap()
            .is_empty());
        let ActiveSubscription::ReadStateChanged(subscription) = subscription else {
            panic!("expected read-state subscription");
        };
        assert_eq!(subscription.last_sequence, event_hub.current_sequence());
    }

    #[test]
    fn agent_status_batch_preserves_transitions_filters_and_initial_state_ordering() {
        for filtered in [false, true] {
            let event_hub = EventHub::default();
            let mut subscription = ActiveSubscription::AgentStatusChanged(Box::new(
                ActiveAgentStatusChangedSubscription {
                    snapshot_requested: false,
                    pane_id: "pane_1".into(),
                    status_filter: filtered.then_some(AgentStatus::Working),
                    last_status: Some(AgentStatus::Working),
                    last_presentation: None,
                    last_sequence: event_hub.current_sequence(),
                    initial_event: Some(PaneAgentStatusChangedEvent {
                        pane_id: "pane_1".into(),
                        workspace_id: "workspace_1".into(),
                        agent_status: AgentStatus::Working,
                        agent: Some("pi".into()),
                        title: Some("stale initial snapshot".into()),
                        display_agent: None,
                        state_labels: HashMap::new(),
                    }),
                    request_prefix: "batch".into(),
                },
            ));
            for (status, title) in [
                (AgentStatus::Working, "started"),
                (AgentStatus::Blocked, "approval"),
                (AgentStatus::Idle, "finished"),
                (AgentStatus::Working, "restarted"),
            ] {
                let mut event = presentation_event(Some(title));
                let EventData::PaneAgentStatusChanged { agent_status, .. } = &mut event.data else {
                    panic!("expected status data");
                };
                *agent_status = status;
                event_hub.push(event);
            }
            let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
            let events = subscription.poll_batch(&api_tx, &event_hub, true).unwrap();
            let titles = events
                .iter()
                .map(|event| event["data"]["title"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                titles,
                if filtered {
                    vec!["started", "restarted"]
                } else {
                    vec!["started", "approval", "finished", "restarted"]
                }
            );
            let ActiveSubscription::AgentStatusChanged(subscription) = subscription else {
                panic!("expected agent subscription");
            };
            assert_eq!(subscription.last_sequence, event_hub.current_sequence());
            assert!(subscription.initial_event.is_none());
        }
    }

    #[test]
    fn scroll_subscription_emits_when_scroll_snapshot_changes() {
        let at_bottom = PaneScrollInfo {
            offset_from_bottom: 0,
            max_offset_from_bottom: 40,
            viewport_rows: 20,
        };
        let scrolled_back = PaneScrollInfo {
            offset_from_bottom: 8,
            max_offset_from_bottom: 40,
            viewport_rows: 20,
        };
        let mut subscription = ActiveScrollChangedSubscription {
            pane_id: "pane_1".into(),
            last_scroll: Some(at_bottom),
            request_prefix: "test".into(),
        };

        assert!(subscription
            .event_from_snapshot(pane_info_with_scroll(Some(at_bottom)))
            .is_none());

        let event = subscription
            .event_from_snapshot(pane_info_with_scroll(Some(scrolled_back)))
            .expect("scroll event");
        assert_eq!(event.event, SubscriptionEventKind::ScrollChanged);
        let SubscriptionEventData::ScrollChanged(data) = event.data else {
            panic!("wrong event data");
        };
        assert_eq!(data.pane_id, "pane_1");
        assert_eq!(data.workspace_id, "workspace_1");
        assert_eq!(data.scroll, scrolled_back);
    }

    #[test]
    fn agent_status_subscription_replays_queued_metadata_set_and_expiry_events() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: None,
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: None,
            snapshot_requested: false,
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));
        event_hub.push(presentation_event(None));

        let set_event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("set event");
        let SubscriptionEventData::PaneAgentStatusChanged(set_data) = set_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(set_data.title.as_deref(), Some("short lived"));

        let expiry_event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("expiry event");
        let SubscriptionEventData::PaneAgentStatusChanged(expiry_data) = expiry_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(expiry_data.title, None);
    }

    #[test]
    fn agent_status_subscription_prefers_setup_window_events_over_initial_snapshot() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            snapshot_requested: false,
            pane_id: "pane_1".into(),
            status_filter: Some(AgentStatus::Working),
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: Some(PaneAgentStatusChangedEvent {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));
        event_hub.push(presentation_event(None));

        let set_event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("set event");
        let SubscriptionEventData::PaneAgentStatusChanged(set_data) = set_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(set_data.title.as_deref(), Some("short lived"));

        let expiry_event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("expiry event");
        let SubscriptionEventData::PaneAgentStatusChanged(expiry_data) = expiry_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(expiry_data.title, None);
    }

    #[test]
    fn agent_status_subscription_emits_setup_window_event_already_reflected_by_probe() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            snapshot_requested: false,
            pane_id: "pane_1".into(),
            status_filter: Some(AgentStatus::Working),
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: Some("short lived".into()),
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: Some(PaneAgentStatusChangedEvent {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: Some("short lived".into()),
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));

        let event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("setup-window event");
        let SubscriptionEventData::PaneAgentStatusChanged(data) = event.data else {
            panic!("wrong event data");
        };
        assert_eq!(data.title.as_deref(), Some("short lived"));
        assert!(subscription.initial_event.is_none());
    }

    fn read_state_event(pane_id: &str, seen: bool, agent_status: AgentStatus) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::PaneReadStateChanged,
            data: EventData::PaneReadStateChanged {
                read_state: PaneReadState {
                    pane_id: pane_id.into(),
                    workspace_id: "w1".into(),
                    seen,
                    agent_status,
                },
            },
        }
    }

    /// Answers the one request a subscription probe sends, as the app would.
    fn answer_one_request(
        mut api_rx: tokio::sync::mpsc::UnboundedReceiver<crate::api::ApiRequestMessage>,
        response: serde_json::Value,
    ) -> std::thread::JoinHandle<Request> {
        std::thread::spawn(move || {
            let message = api_rx.blocking_recv().expect("probe request");
            message
                .respond_to
                .send(response.to_string())
                .expect("probe response");
            message.request
        })
    }

    #[test]
    fn read_state_subscription_relays_its_pane_with_the_exact_read_state_body() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveReadStateChangedSubscription {
            pane_id: "w1:p1".into(),
            last_sequence: event_hub.current_sequence(),
        };

        event_hub.push(read_state_event("w1:p2", false, AgentStatus::Blocked));
        event_hub.push(presentation_event(None));
        event_hub.push(read_state_event("w1:p1", false, AgentStatus::Blocked));
        event_hub.push(read_state_event("w1:p1", true, AgentStatus::Blocked));

        let first = subscription.poll(&event_hub).expect("unread event");
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            serde_json::json!({
                "event": "pane.read_state_changed",
                "data": {
                    "pane_id": "w1:p1",
                    "workspace_id": "w1",
                    "seen": false,
                    "agent_status": "blocked",
                }
            }),
            "the body must be the pane.read_state result"
        );
        let second = subscription.poll(&event_hub).expect("read event");
        let SubscriptionEventData::PaneReadStateChanged(body) = second.data else {
            panic!("wrong event data");
        };
        assert!(body.seen);
        assert!(
            subscription.poll(&event_hub).is_none(),
            "other panes and other event kinds are not relayed"
        );
    }

    #[test]
    fn read_state_subscription_resolves_the_pane_and_skips_history() {
        let event_hub = EventHub::default();
        event_hub.push(read_state_event("w1:p1", false, AgentStatus::Blocked));
        let event_start_sequence = event_hub.current_sequence();
        event_hub.push(read_state_event("w1:p1", true, AgentStatus::Blocked));

        let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let probe = answer_one_request(
            api_rx,
            serde_json::json!({
                "id": "test:sub:0:probe",
                "result": {
                    "type": "pane_read_state",
                    "read_state": {
                        "pane_id": "w1:p1",
                        "workspace_id": "w1",
                        "seen": true,
                        "agent_status": "blocked",
                    }
                }
            }),
        );
        // The legacy id form resolves to the canonical id the events carry.
        let mut subscription = ActiveSubscription::new(
            Subscription::PaneReadStateChanged {
                pane_id: "w1-1".into(),
            },
            "test",
            0,
            &api_tx,
            &event_hub,
            event_start_sequence,
        )
        .expect("read state subscription");

        let request = probe.join().unwrap();
        assert_eq!(
            serde_json::to_value(&request).unwrap()["method"],
            "pane.read_state"
        );
        let setup_window_event = subscription
            .poll(&api_tx, &event_hub)
            .expect("an event after the request started is kept");
        assert_eq!(setup_window_event["event"], "pane.read_state_changed");
        assert!(
            subscription.poll(&api_tx, &event_hub).is_none(),
            "an event from before the request is history"
        );
        event_hub.push(read_state_event("w1:p1", false, AgentStatus::Blocked));
        assert!(subscription.poll(&api_tx, &event_hub).is_some());
    }

    #[test]
    fn read_state_subscription_for_an_unknown_pane_fails_loudly() {
        let event_hub = EventHub::default();
        let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let probe = answer_one_request(
            api_rx,
            serde_json::json!({
                "id": "test:sub:0:probe",
                "error": { "code": "pane_not_found", "message": "pane missing not found" }
            }),
        );

        let result = ActiveSubscription::new(
            Subscription::PaneReadStateChanged {
                pane_id: "missing".into(),
            },
            "test",
            0,
            &api_tx,
            &event_hub,
            event_hub.current_sequence(),
        );

        probe.join().unwrap();
        let Err(error) = result else {
            panic!("an unknown pane must not start a subscription");
        };
        assert_eq!(error.error.code, "pane_not_found");
    }
}
