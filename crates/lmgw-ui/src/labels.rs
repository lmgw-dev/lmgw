//! Words for the keys the gateway's data carries: a config key is never a label.

/// A key as a label: the common abbreviations spelled out (`tts`, `asr`,
/// `sfx`), any other key with its underscores as spaces and a capital.
pub fn category_label(key: &str) -> String {
    match key {
        "tts" => return "Text to speech".into(),
        "asr" => return "Transcription".into(),
        "sfx" => return "Sound effects".into(),
        _ => {}
    }
    capitalise(&key.replace('_', " "))
}

/// An `error_kind` of the request log as words: the codes lmgw itself
/// produces spelled out, any other code through [`category_label`]. The code
/// stays the tooltip of whatever shows the words.
pub fn error_kind_label(code: &str) -> String {
    let words = match code {
        "unknown_alias" => "Unknown model",
        "not_found" => "Not found",
        "auth" | "unauthorized" => "Not signed in or bad key",
        "agent_disabled" => "Agent disabled",
        "bad_request" => "Bad request",
        "body_limit" => "Request body too large",
        "upstream" => "Upstream error",
        "transport" => "Upstream connection failed",
        "unreachable" => "Upstream unreachable",
        "timeout" => "Timed out",
        "unsupported" => "Not supported",
        "vram" => "Not enough video memory",
        "vram_too_large" => "Model too large for the card",
        "vram_queue_timeout" => "Waited too long for the card",
        "internal" => "Internal error",
        "gpu_hold" => "GPU on hold",
        "gpu_benchmark" => "Card held for a benchmark",
        "gpu_setup" => "Graphics card not set up",
        "engine_download" => "Engine still downloading",
        "gpu_deferred" => "Deferred to spare the card",
        "candidate_lost" => "Candidate model lost",
        "candidate_unavailable" => "No candidate model available",
        "key_scope" => "Outside the key's scope",
        "key_budget" => "Key budget spent",
        "budget_unpriced" => "No price for a budgeted key",
        "key_rate" => "Key rate limit reached",
        "key_expired" => "Key expired",
        "rate_limit_exceeded" => "Rate limit reached",
        "insufficient_quota" => "Out of quota",
        "resource_unavailable" => "No flex capacity",
        "context_length_exceeded" => "Context too long",
        "response_format_unsupported" => "Response format not supported",
        "invalid_json_schema" => "Invalid JSON schema",
        "cross_origin_refused" => "Cross-origin request refused",
        "canceled" => "Canceled",
        other => return category_label(other),
    };
    words.to_string()
}

/// A string with its first letter upper case (`day` → `Day`, `no agent with id
/// 'x'` → `No agent with id 'x'`): a reason, a server message or a key where it
/// starts a sentence or a row title.
pub fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// A compute backend's key (`cuda`, `cpu`) as its proper name. The settings
/// list spells the same names as constants and a test holds them to this.
pub fn backend_label(key: &str) -> String {
    match key {
        "cuda" => "CUDA".into(),
        "cpu" => "CPU".into(),
        "vulkan" => "Vulkan".into(),
        "metal" => "Metal".into(),
        "hip" => "HIP".into(),
        "rocm" => "ROCm".into(),
        other => category_label(other),
    }
}

/// An image model's mode (`img_gen`, `vid_gen`) as the image editor's
/// checkboxes word it.
pub fn image_mode_label(mode: &str) -> String {
    match mode {
        "img_gen" => "Images".into(),
        "vid_gen" => "Video".into(),
        other => category_label(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_names_are_proper_names() {
        assert_eq!(backend_label("cuda"), "CUDA");
        assert_eq!(backend_label("vulkan"), "Vulkan");
        assert_eq!(backend_label("rocm"), "ROCm");
    }

    #[test]
    fn keys_become_words() {
        assert_eq!(category_label("tts"), "Text to speech");
        assert_eq!(category_label("asr"), "Transcription");
        assert_eq!(category_label("audio_generation"), "Audio generation");
        assert_eq!(category_label("offline"), "Offline");
        assert_eq!(category_label(""), "");
        assert_eq!(capitalise("no agent with id 'x'"), "No agent with id 'x'");
        assert_eq!(capitalise("Already"), "Already");
        assert_eq!(capitalise(""), "");
        assert_eq!(image_mode_label("img_gen"), "Images");
        assert_eq!(image_mode_label("vid_gen"), "Video");
    }

    #[test]
    fn error_kinds_have_words() {
        for (code, words) in [
            ("unknown_alias", "Unknown model"),
            ("not_found", "Not found"),
            ("auth", "Not signed in or bad key"),
            ("unauthorized", "Not signed in or bad key"),
            ("agent_disabled", "Agent disabled"),
            ("bad_request", "Bad request"),
            ("body_limit", "Request body too large"),
            ("upstream", "Upstream error"),
            ("transport", "Upstream connection failed"),
            ("unreachable", "Upstream unreachable"),
            ("timeout", "Timed out"),
            ("unsupported", "Not supported"),
            ("vram", "Not enough video memory"),
            ("vram_too_large", "Model too large for the card"),
            ("vram_queue_timeout", "Waited too long for the card"),
            ("internal", "Internal error"),
            ("gpu_hold", "GPU on hold"),
            ("gpu_benchmark", "Card held for a benchmark"),
            ("gpu_setup", "Graphics card not set up"),
            ("engine_download", "Engine still downloading"),
            ("gpu_deferred", "Deferred to spare the card"),
            ("candidate_lost", "Candidate model lost"),
            ("candidate_unavailable", "No candidate model available"),
            ("key_scope", "Outside the key's scope"),
            ("key_budget", "Key budget spent"),
            ("budget_unpriced", "No price for a budgeted key"),
            ("key_rate", "Key rate limit reached"),
            ("key_expired", "Key expired"),
            ("rate_limit_exceeded", "Rate limit reached"),
            ("insufficient_quota", "Out of quota"),
            ("resource_unavailable", "No flex capacity"),
            ("context_length_exceeded", "Context too long"),
            (
                "response_format_unsupported",
                "Response format not supported",
            ),
            ("invalid_json_schema", "Invalid JSON schema"),
            ("cross_origin_refused", "Cross-origin request refused"),
            ("canceled", "Canceled"),
        ] {
            assert_eq!(error_kind_label(code), words, "{code}");
        }
    }

    #[test]
    fn an_unknown_error_kind_falls_back_to_its_key() {
        assert_eq!(error_kind_label("some_new_kind"), "Some new kind");
        assert_eq!(error_kind_label(""), "");
    }
}
