//! Target-neutral HTTP failure and background-job policy.
#![cfg_attr(test, allow(dead_code))]

use serde::Deserialize;

/// Target-neutral facts from a failed HTTP exchange.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HttpFailure<'a> {
    pub(crate) status: u16,
    pub(crate) status_text: &'a str,
    pub(crate) body: Option<&'a str>,
}

/// User-facing context for a failed browser request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HttpRequestContext {
    Upload,
    JobStatus,
    Download,
    CancelJob,
    Layout,
    Preview,
    RenewSource,
    DeleteSource,
}

impl HttpRequestContext {
    const fn failure_label(self) -> &'static str {
        match self {
            Self::Upload => "Upload",
            Self::JobStatus => "Reading job status",
            Self::Download => "Download",
            Self::CancelJob => "Cancelling the job",
            Self::Layout => "Calculating the sheet layout",
            Self::Preview => "Rendering artwork previews",
            Self::RenewSource => "Renewing the prepared source",
            Self::DeleteSource => "Deleting the prepared source",
        }
    }

    const fn transport_action(self) -> &'static str {
        match self {
            Self::Upload => "upload files",
            Self::JobStatus => "read the job status",
            Self::Download => "download the job result",
            Self::CancelJob => "cancel the server job",
            Self::Layout => "calculate the sheet layout",
            Self::Preview => "render artwork previews",
            Self::RenewSource => "renew the prepared source",
            Self::DeleteSource => "delete the prepared source",
        }
    }

    const fn transport_recovery(self) -> &'static str {
        match self {
            Self::JobStatus | Self::Download => "Check the connection and run the job again.",
            Self::CancelJob => "Check the connection; server work may continue until it expires.",
            Self::RenewSource => "Check the connection and retry before the source expires.",
            Self::DeleteSource => {
                "Check the connection; the prepared source will be removed when it expires."
            }
            Self::Upload | Self::Layout | Self::Preview => "Check the connection and retry.",
        }
    }
}

/// Formats every HTTP rejection without treating an empty 5xx response as a transport failure.
pub(crate) fn http_failure_message(
    context: HttpRequestContext,
    failure: HttpFailure<'_>,
) -> String {
    if failure.status == 0 {
        return format!(
            "Could not {} because the PDF service could not be reached. {}",
            context.transport_action(),
            context.transport_recovery(),
        );
    }
    let status_text = failure.status_text.trim();
    let status_label = if status_text.is_empty() {
        failure.status.to_string()
    } else {
        format!("{} {status_text}", failure.status)
    };
    match failure.body.map(str::trim).filter(|body| !body.is_empty()) {
        Some(body) => format!(
            "{} failed (HTTP {status_label}): {body}",
            context.failure_label()
        ),
        None => format!(
            "{} failed (HTTP {status_label}) without an error message.",
            context.failure_label()
        ),
    }
}

/// Server job phase.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum JobPhase {
    /// Waiting for execution.
    Queued,
    /// Actively processing.
    Running,
    /// Result is available.
    Done,
    /// Terminal failure.
    Error,
}

/// Parsed line-based status returned by the existing Axum API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JobStatus {
    /// Current phase.
    pub(crate) phase: JobPhase,
    /// Validated percent supplied by the backend.
    pub(crate) percent: Option<u8>,
    /// Optional progress stage.
    pub(crate) stage: Option<String>,
    /// Suggested filename.
    pub(crate) filename: Option<String>,
    /// Failure detail.
    pub(crate) error: Option<String>,
}

/// Parses the backend's newline-delimited job status response.
pub(crate) fn parse_job_status(value: &str) -> Result<JobStatus, &'static str> {
    let field = |key: &str| {
        value.lines().find_map(|line| {
            let (name, value) = line.split_once('=')?;
            (name == key).then(|| value.to_owned())
        })
    };
    let phase = match field("status").as_deref() {
        Some("queued") => JobPhase::Queued,
        Some("running") => JobPhase::Running,
        Some("done") => JobPhase::Done,
        Some("error") => JobPhase::Error,
        _ => return Err("The server returned an invalid job status."),
    };
    let percent = field("percent")
        .map(|value| {
            value
                .parse::<u8>()
                .ok()
                .filter(|percent| *percent <= 100)
                .ok_or("The server returned an invalid job progress percentage.")
        })
        .transpose()?;
    Ok(JobStatus {
        phase,
        percent,
        stage: field("stage"),
        filename: field("filename"),
        error: field("error"),
    })
}

/// Poll backoff, capped to keep cancellation responsive.
pub(crate) fn job_poll_delay(base_millis: u32, poll_count: u32) -> u32 {
    let exponent = poll_count / 10;
    let multiplier = 2_u32.saturating_pow(exponent);
    base_millis.max(1).saturating_mul(multiplier).min(500)
}

/// Explains the current server phase without replacing its precise stage label.
pub(crate) fn job_progress_detail(stage: &str, filename: Option<&str>) -> String {
    if stage.starts_with("Converted image") {
        "Image decoded and normalized; preserving upload order.".into()
    } else if stage.starts_with("Built image PDF page") {
        "Adding the converted image to the reusable PDF source.".into()
    } else if stage.contains("artwork geometry") {
        "Checking every artwork page for the same finished size and orientation.".into()
    } else if stage.starts_with("Merged ") || stage == "Writing merged PDF" {
        "Combining prepared artwork in upload order.".into()
    } else if stage.starts_with("Inspected page")
        || stage == "Reading PDF structure"
        || stage == "Detecting trim size and bleed"
    {
        "Inspecting page size, orientation, trim, and bleed.".into()
    } else if stage == "Opening source artwork"
        || stage.starts_with("Prepared ")
        || stage == "Source artwork is ready"
        || stage == "Saving source PDF"
    {
        "Preparing the merged artwork for reusable previews and export.".into()
    } else if stage == "Upload staged on disk" {
        "Upload complete; source preparation can begin.".into()
    } else {
        filename
            .map(|filename| format!("Preparing {filename}"))
            .unwrap_or_else(|| "Preparing the reusable artwork source.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_failures_distinguish_server_rejections_from_transport_failures() {
        assert_eq!(
            http_failure_message(
                HttpRequestContext::Upload,
                HttpFailure {
                    status: 413,
                    status_text: "Payload Too Large",
                    body: Some("upload exceeds the configured limit"),
                }
            ),
            "Upload failed (HTTP 413 Payload Too Large): upload exceeds the configured limit"
        );
        assert_eq!(
            http_failure_message(
                HttpRequestContext::Layout,
                HttpFailure {
                    status: 500,
                    status_text: "Internal Server Error",
                    body: Some("  "),
                }
            ),
            "Calculating the sheet layout failed (HTTP 500 Internal Server Error) without an error message."
        );
        assert_eq!(
            http_failure_message(
                HttpRequestContext::Preview,
                HttpFailure {
                    status: 0,
                    status_text: "",
                    body: None,
                }
            ),
            "Could not render artwork previews because the PDF service could not be reached. Check the connection and retry."
        );
        for context in [
            HttpRequestContext::JobStatus,
            HttpRequestContext::Download,
            HttpRequestContext::CancelJob,
            HttpRequestContext::RenewSource,
            HttpRequestContext::DeleteSource,
        ] {
            let message = http_failure_message(
                context,
                HttpFailure {
                    status: 502,
                    status_text: "Bad Gateway",
                    body: None,
                },
            );
            assert!(message.contains("HTTP 502 Bad Gateway"));
            assert!(!message.contains("could not be reached"));
        }
        assert_eq!(
            http_failure_message(
                HttpRequestContext::Download,
                HttpFailure {
                    status: 0,
                    status_text: "",
                    body: None,
                }
            ),
            "Could not download the job result because the PDF service could not be reached. Check the connection and run the job again."
        );
        assert_eq!(
            http_failure_message(
                HttpRequestContext::CancelJob,
                HttpFailure {
                    status: 0,
                    status_text: "",
                    body: None,
                }
            ),
            "Could not cancel the server job because the PDF service could not be reached. Check the connection; server work may continue until it expires."
        );
    }

    #[test]
    fn line_status_and_backoff_match_backend_contract() {
        let status = parse_job_status(
            "status=done\npercent=100\nstage=Complete\nfilename=result.zip\ncontent_type=application/zip",
        );
        assert!(matches!(
            status,
            Ok(JobStatus {
                phase: JobPhase::Done,
                ..
            })
        ));
        assert_eq!(job_poll_delay(100, 0), 100);
        assert_eq!(job_poll_delay(100, 10), 200);
        assert_eq!(job_poll_delay(100, 30), 500);
    }

    #[test]
    fn job_status_rejects_malformed_or_out_of_range_progress() {
        assert_eq!(
            parse_job_status("status=running\npercent=working"),
            Err("The server returned an invalid job progress percentage.")
        );
        assert_eq!(
            parse_job_status("status=running\npercent=101"),
            Err("The server returned an invalid job progress percentage.")
        );
        assert_eq!(
            parse_job_status("status=queued\nstage=Waiting to start"),
            Ok(JobStatus {
                phase: JobPhase::Queued,
                percent: None,
                stage: Some("Waiting to start".into()),
                filename: None,
                error: None,
            })
        );
    }
}
