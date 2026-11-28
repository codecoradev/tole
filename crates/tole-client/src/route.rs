//! The routes of `tole serve` the client has a method for.

/// The serve routes this client has a method for. A change on the server that
/// removes one fails the contract test, which probes every entry of [`ALL`].
///
/// [`ALL`]: Route::ALL
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Health,
    Info,
    Sessions,
    SessionIndex,
    CreateSession,
    Prompt,
    Cancel,
    Status,
    Entries,
    Events,
    Approvals,
    Decide,
    Models,
    Discussions,
    Discussion,
    DiscussionEvents,
    DiscussionEstimate,
    StartDiscussion,
    ResumeDiscussion,
    StopDiscussion,
    CronJobs,
    PauseCronJob,
    ResumeCronJob,
    CronRuns,
    Profiles,
    Skills,
    Tools,
    Interrupt,
    Detach,
    Fork,
    Tree,
    Compact,
    Name,
    SetModel,
    Effort,
    UploadImage,
    ReadImage,
    StartMission,
    Mission,
    CancelMission,
}

impl Route {
    pub const ALL: [Route; 40] = [
        Route::Health,
        Route::Info,
        Route::Sessions,
        Route::SessionIndex,
        Route::CreateSession,
        Route::Prompt,
        Route::Cancel,
        Route::Status,
        Route::Entries,
        Route::Events,
        Route::Approvals,
        Route::Decide,
        Route::Models,
        Route::Discussions,
        Route::Discussion,
        Route::DiscussionEvents,
        Route::DiscussionEstimate,
        Route::StartDiscussion,
        Route::ResumeDiscussion,
        Route::StopDiscussion,
        Route::CronJobs,
        Route::PauseCronJob,
        Route::ResumeCronJob,
        Route::CronRuns,
        Route::Profiles,
        Route::Skills,
        Route::Tools,
        Route::Interrupt,
        Route::Detach,
        Route::Fork,
        Route::Tree,
        Route::Compact,
        Route::Name,
        Route::SetModel,
        Route::Effort,
        Route::UploadImage,
        Route::ReadImage,
        Route::StartMission,
        Route::Mission,
        Route::CancelMission,
    ];

    pub fn method(self) -> &'static str {
        match self {
            Route::CreateSession
            | Route::Prompt
            | Route::Cancel
            | Route::Decide
            | Route::DiscussionEstimate
            | Route::StartDiscussion
            | Route::ResumeDiscussion
            | Route::StopDiscussion
            | Route::PauseCronJob
            | Route::ResumeCronJob
            | Route::Interrupt
            | Route::Detach
            | Route::Fork
            | Route::Compact
            | Route::Name
            | Route::SetModel
            | Route::Effort
            | Route::UploadImage
            | Route::StartMission
            | Route::CancelMission => "POST",
            _ => "GET",
        }
    }

    /// The path, with `{id}` where the session, approval, discussion, cron job
    /// or mission id goes, and `{hash}` where an image's hash goes.
    pub fn template(self) -> &'static str {
        match self {
            Route::Health => "/health",
            Route::Info => "/info",
            Route::Sessions => "/sessions",
            Route::SessionIndex => "/sessions/index",
            Route::CreateSession => "/sessions",
            Route::Prompt => "/sessions/{id}/prompt",
            Route::Cancel => "/sessions/{id}/cancel",
            Route::Status => "/sessions/{id}/status",
            Route::Entries => "/sessions/{id}/entries",
            Route::Events => "/sessions/{id}/events",
            Route::Approvals => "/approvals",
            Route::Decide => "/approvals/{id}/decision",
            Route::Models => "/models",
            Route::Discussions => "/discussions",
            Route::Discussion => "/discussions/{id}",
            Route::DiscussionEvents => "/discussions/{id}/events",
            Route::DiscussionEstimate => "/discussions/estimate",
            Route::StartDiscussion => "/discussions",
            Route::ResumeDiscussion => "/discussions/{id}/resume",
            Route::StopDiscussion => "/discussions/{id}/stop",
            Route::CronJobs => "/cron/jobs",
            Route::PauseCronJob => "/cron/jobs/{id}/pause",
            Route::ResumeCronJob => "/cron/jobs/{id}/resume",
            Route::CronRuns => "/cron/runs",
            Route::Profiles => "/profiles",
            Route::Skills => "/skills",
            Route::Tools => "/tools",
            Route::Interrupt => "/sessions/{id}/interrupt",
            Route::Detach => "/sessions/{id}/detach",
            Route::Fork => "/sessions/{id}/fork",
            Route::Tree => "/sessions/{id}/tree",
            Route::Compact => "/sessions/{id}/compact",
            Route::Name => "/sessions/{id}/name",
            Route::SetModel => "/sessions/{id}/model",
            Route::Effort => "/sessions/{id}/effort",
            Route::UploadImage => "/sessions/{id}/images",
            Route::ReadImage => "/sessions/{id}/images/{hash}",
            Route::StartMission => "/missions",
            Route::Mission => "/missions/{id}",
            Route::CancelMission => "/missions/{id}/cancel",
        }
    }
}
