// Output guard — protects against output injection attacks.
//
// When tool output contains text that looks like system instructions or
// agent directives, it can be exploited to manipulate the agent. The output
// guard wraps suspicious output in markers that prevent the LLM from
// interpreting it as instructions.

/// Guard wrapping markers for tool output.
const GUARD_START: &str = "<tool_output>";
const GUARD_END: &str = "</tool_output>";

/// Configuration for output guarding.
#[derive(Debug, Clone)]
pub struct OutputGuardConfig {
    /// Whether output guarding is enabled.
    pub enabled: bool,
    /// Maximum length of guarded output before truncation.
    pub max_guarded_length: usize,
}

impl Default for OutputGuardConfig {
    fn default() -> Self {
        OutputGuardConfig {
            enabled: true,
            max_guarded_length: 256 * 1024, // 256 KiB
        }
    }
}

/// Guard tool output by wrapping it in markers that prevent instruction
/// injection.
///
/// The guard also truncates the output if it exceeds the configured maximum
/// length.
pub fn guard_output(output: &str, config: &OutputGuardConfig) -> String {
    if !config.enabled {
        return output.to_string();
    }

    let truncated = if output.len() > config.max_guarded_length {
        let half = config.max_guarded_length / 2;
        let head = &output[..half];
        let tail = &output[output.len() - half..];
        let skipped = output.len() - config.max_guarded_length;
        format!("{head}\n... [{skipped} bytes omitted] ...\n{tail}")
    } else {
        output.to_string()
    };

    format!("{GUARD_START}\n{truncated}\n{GUARD_END}")
}

/// Guard multiple tool outputs, joining them with separators.
pub fn guard_outputs(outputs: &[&str], config: &OutputGuardConfig) -> String {
    if !config.enabled {
        return outputs.join("\n---\n");
    }

    outputs
        .iter()
        .map(|o| guard_output(o, config))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guard_output() {
        let config = OutputGuardConfig::default();
        let guarded = guard_output("hello world", &config);
        assert!(guarded.contains(GUARD_START));
        assert!(guarded.contains(GUARD_END));
        assert!(guarded.contains("hello world"));
    }

    #[test]
    fn test_guard_disabled() {
        let config = OutputGuardConfig {
            enabled: false,
            ..Default::default()
        };
        let guarded = guard_output("hello world", &config);
        assert_eq!(guarded, "hello world");
    }

    #[test]
    fn test_guard_outputs() {
        let config = OutputGuardConfig::default();
        let guarded = guard_outputs(&["out1", "out2"], &config);
        assert!(guarded.contains(GUARD_START));
        assert!(guarded.contains("out1"));
        assert!(guarded.contains("out2"));
    }
}
