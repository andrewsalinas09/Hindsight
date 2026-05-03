// SPDX-License-Identifier: Apache-2.0

//! Trace metadata block (initial metadata, written after the file header).
//!
//! See `docs/trace-format.md` §"Initial metadata block". The payload is TOML
//! (format tag 0x01). The schema here is small and fixed, so we hand-roll the
//! TOML output rather than pulling in `toml`/`serde`.

use std::fmt::Write as _;

#[derive(Debug, Clone)]
pub struct Metadata {
    pub recorder: RecorderInfo,
    pub recording: RecordingInfo,
    pub program: Option<ProgramInfo>,
    /// Random 128-bit identifier for this trace. Goes into the file header.
    pub trace_uuid: [u8; 16],
    /// Wall-clock recording start, ns since Unix epoch. Goes into the file header.
    pub recording_start_ns: u64,
}

#[derive(Debug, Clone)]
pub struct RecorderInfo {
    pub language: String,
    pub language_version: String,
    pub recorder_version: String,
    pub platform: String,
}

#[derive(Debug, Clone)]
pub struct RecordingInfo {
    pub program: String,
    pub working_directory: Option<String>,
    pub scope_config: ScopeConfig,
}

#[derive(Debug, Clone, Default)]
pub struct ScopeConfig {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// `None` means unlimited depth.
    /// TODO(spec): the trace-format example writes `depth_limit = null`, which
    /// isn't valid TOML. We omit the key when None — update the spec to match.
    pub depth_limit: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct ProgramInfo {
    /// Free-form key/value pairs (e.g., project name, git commit).
    pub fields: Vec<(String, String)>,
}

impl Metadata {
    /// Render the metadata payload as TOML bytes.
    pub fn to_toml(&self) -> String {
        let mut s = String::new();

        s.push_str("[recorder]\n");
        write_string_kv(&mut s, "language", &self.recorder.language);
        write_string_kv(&mut s, "language_version", &self.recorder.language_version);
        write_string_kv(&mut s, "recorder_version", &self.recorder.recorder_version);
        write_string_kv(&mut s, "platform", &self.recorder.platform);
        s.push('\n');

        s.push_str("[recording]\n");
        write_string_kv(&mut s, "program", &self.recording.program);
        if let Some(wd) = &self.recording.working_directory {
            write_string_kv(&mut s, "working_directory", wd);
        }
        s.push('\n');

        s.push_str("[recording.scope_config]\n");
        write_string_array_kv(&mut s, "include", &self.recording.scope_config.include);
        write_string_array_kv(&mut s, "exclude", &self.recording.scope_config.exclude);
        if let Some(n) = self.recording.scope_config.depth_limit {
            writeln!(s, "depth_limit = {n}").expect("write to String never fails");
        }

        if let Some(program) = &self.program
            && !program.fields.is_empty()
        {
            s.push('\n');
            s.push_str("[program]\n");
            for (k, v) in &program.fields {
                write_string_kv(&mut s, k, v);
            }
        }

        s
    }
}

fn write_string_kv(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(" = ");
    write_basic_string(out, value);
    out.push('\n');
}

fn write_string_array_kv(out: &mut String, key: &str, values: &[String]) {
    out.push_str(key);
    out.push_str(" = [");
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_basic_string(out, v);
    }
    out.push_str("]\n");
}

/// TOML basic string: double-quoted, with the escape rules from the TOML spec.
fn write_basic_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                write!(out, "\\u{:04X}", c as u32).expect("write to String never fails");
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Metadata {
        Metadata {
            recorder: RecorderInfo {
                language: "python".to_string(),
                language_version: "3.12.5".to_string(),
                recorder_version: "0.1.0".to_string(),
                platform: "linux-x86_64".to_string(),
            },
            recording: RecordingInfo {
                program: "python script.py".to_string(),
                working_directory: Some("/tmp/proj".to_string()),
                scope_config: ScopeConfig {
                    include: vec![],
                    exclude: vec!["defaults".to_string(), "myapp.helpers.*".to_string()],
                    depth_limit: None,
                },
            },
            program: None,
            trace_uuid: [0; 16],
            recording_start_ns: 0,
        }
    }

    #[test]
    fn toml_renders_required_sections() {
        let toml = fixture().to_toml();
        assert!(toml.contains("[recorder]"));
        assert!(toml.contains("language = \"python\""));
        assert!(toml.contains("[recording]"));
        assert!(toml.contains("program = \"python script.py\""));
        assert!(toml.contains("[recording.scope_config]"));
        assert!(toml.contains("include = []"));
        assert!(toml.contains("exclude = [\"defaults\", \"myapp.helpers.*\"]"));
    }

    #[test]
    fn toml_omits_depth_limit_when_none() {
        let toml = fixture().to_toml();
        assert!(!toml.contains("depth_limit"));
    }

    #[test]
    fn toml_writes_depth_limit_when_set() {
        let mut m = fixture();
        m.recording.scope_config.depth_limit = Some(2);
        let toml = m.to_toml();
        assert!(toml.contains("depth_limit = 2"));
    }

    #[test]
    fn toml_omits_working_directory_when_none() {
        let mut m = fixture();
        m.recording.working_directory = None;
        let toml = m.to_toml();
        assert!(!toml.contains("working_directory"));
    }

    #[test]
    fn toml_program_section_only_when_fields_present() {
        let toml = fixture().to_toml();
        assert!(!toml.contains("[program]"));

        let mut m = fixture();
        m.program = Some(ProgramInfo {
            fields: vec![("git_commit".to_string(), "abc123".to_string())],
        });
        let toml = m.to_toml();
        assert!(toml.contains("[program]"));
        assert!(toml.contains("git_commit = \"abc123\""));
    }

    #[test]
    fn toml_string_escapes_control_chars_and_quotes() {
        let mut m = fixture();
        m.recording.program = "say \"hi\"\nthen\ttab".to_string();
        let toml = m.to_toml();
        assert!(toml.contains(r#"program = "say \"hi\"\nthen\ttab""#));
    }
}
