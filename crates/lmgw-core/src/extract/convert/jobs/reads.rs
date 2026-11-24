//! The requests that only look at a job or end it (document-conversion
//! design §7.3, §7.4.1), which the key gate admits as it admits a call's
//! hangup: the key's expiry is checked, and no concurrency slot is taken and
//! no request counted. The work is the job's: the request that made it
//! handed its slot to the job (`jobs::Start::slot`), and every poll of a
//! slow job would otherwise spend the key's requests per minute or hold
//! its only slot for the whole `wait`.

/// Whether `method` on `path` (the whole path, `/v1` included) reads or
/// cancels a job: the native job, its result and its cancel, and docling's
/// poll, status WebSocket and result routes.
pub fn is_job_read(method: &str, path: &str) -> bool {
    let one = |prefix: &str| {
        path.strip_prefix(prefix)
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
    };
    let job_result = || {
        path.strip_prefix("/v1/documents/jobs/")
            .and_then(|r| r.strip_suffix("/result"))
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
    };
    match method {
        "GET" => {
            one("/v1/documents/jobs/")
                || job_result()
                || one("/v1/status/poll/")
                || one("/v1/status/ws/")
                || one("/v1/result/")
        }
        "DELETE" => one("/v1/documents/jobs/"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_job_routes_that_look_or_cancel() {
        for (m, p) in [
            ("GET", "/v1/documents/jobs/cj_01"),
            ("GET", "/v1/documents/jobs/cj_01/result"),
            ("DELETE", "/v1/documents/jobs/cj_01"),
            ("GET", "/v1/status/poll/cj_01"),
            ("GET", "/v1/status/ws/cj_01"),
            ("GET", "/v1/result/cj_01"),
        ] {
            assert!(is_job_read(m, p), "{m} {p}");
        }
        for (m, p) in [
            ("POST", "/v1/documents/convert"),
            ("POST", "/v1/convert/file/async"),
            ("POST", "/v1/documents/jobs/cj_01"),
            ("GET", "/v1/documents/jobs/"),
            ("GET", "/v1/documents/jobs/cj_01/x/y"),
            ("DELETE", "/v1/result/cj_01"),
            ("GET", "/v1/status/poll/"),
        ] {
            assert!(!is_job_read(m, p), "{m} {p}");
        }
    }
}
