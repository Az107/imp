//! `read_file`: bounded, line-numbered reads inside the workspace.

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use minion_core::error::{Error, Result};
use minion_core::tool::{Risk, Tool, ToolCtx, ToolOutput};

/// Lines returned when the caller does not say.
const DEFAULT_LIMIT: usize = 2_000;
/// Hard ceiling on `limit`, to keep one tool result from flooding the context.
const MAX_LIMIT: usize = 20_000;

/// Arguments accepted by [`ReadFile`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Path to the file. Relative paths resolve against the workspace root.
    pub path: String,
    /// 1-based line to start from. Defaults to the first line.
    pub offset: Option<usize>,
    /// Maximum number of lines to return. Defaults to 2000.
    pub limit: Option<usize>,
}

/// Reads UTF-8 text files, refusing anything outside the workspace or oversized.
pub struct ReadFile {
    /// Largest file this tool will read, in bytes.
    pub max_bytes: u64,
}

impl ReadFile {
    /// Build the tool with a byte cap taken from `workspace.max_file_bytes`.
    pub fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file from the workspace. Returns numbered lines. Use offset and limit for large files."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ReadFileArgs)).unwrap_or_default()
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    async fn invoke(&self, ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput> {
        let args: ReadFileArgs = serde_json::from_value(args).map_err(|err| Error::ToolArgs {
            tool: self.name().to_string(),
            message: err.to_string(),
        })?;

        let path = ctx.resolve(&args.path)?;

        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|err| Error::Tool {
                tool: self.name().to_string(),
                message: format!("cannot stat `{}`: {err}", args.path),
            })?;
        if !metadata.is_file() {
            return Err(Error::Tool {
                tool: self.name().to_string(),
                message: format!("`{}` is not a regular file", args.path),
            });
        }
        if metadata.len() > self.max_bytes {
            return Err(Error::Tool {
                tool: self.name().to_string(),
                message: format!(
                    "`{}` is {} bytes, over the {} byte read cap",
                    args.path,
                    metadata.len(),
                    self.max_bytes
                ),
            });
        }

        let bytes = tokio::fs::read(&path).await?;
        let content = String::from_utf8(bytes).map_err(|_| Error::Tool {
            tool: self.name().to_string(),
            message: format!("`{}` is not valid UTF-8", args.path),
        })?;

        let total_lines = content.lines().count();
        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        let selected: Vec<&str> = content.lines().skip(offset - 1).take(limit).collect();
        let shown = selected.len();
        let last_line = offset - 1 + shown;
        let truncated = last_line < total_lines;

        let width = last_line.to_string().len();
        let mut body = selected
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{:>width$}\t{line}", offset + index, width = width))
            .collect::<Vec<_>>()
            .join("\n");

        if truncated {
            body.push_str(&format!(
                "\n... truncated: showed lines {offset}-{last_line} of {total_lines}"
            ));
        }

        let metadata = serde_json::json!({
            "path": path,
            "bytes": metadata.len(),
            "total_lines": total_lines,
            "shown_lines": shown,
            "truncated": truncated,
        });

        Ok(ToolOutput {
            content: body,
            truncated,
            metadata,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    fn ctx(root: &Path) -> ToolCtx {
        ToolCtx {
            workspace_root: root.to_path_buf(),
            cancel: CancellationToken::new(),
        }
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[tokio::test]
    async fn reads_a_file_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "note.txt", "alpha\nbeta\n");

        let output = ReadFile::new(1024)
            .invoke(ctx(dir.path()), serde_json::json!({ "path": "note.txt" }))
            .await
            .unwrap();

        assert!(output.content.contains("alpha"));
        assert!(output.content.contains("beta"));
        // Lines are numbered from one.
        assert!(
            output.content.contains("1\talpha"),
            "missing numbering: {}",
            output.content
        );
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn rejects_paths_that_escape_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let err = ReadFile::new(1024)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "path": "../../etc/passwd" }),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, Error::Denied(_)), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn rejects_absolute_paths_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let err = ReadFile::new(1024)
            .invoke(ctx(dir.path()), serde_json::json!({ "path": "/etc/hosts" }))
            .await
            .unwrap_err();

        assert!(matches!(err, Error::Denied(_)), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn honours_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "many.txt",
            &(1..=10).map(|n| format!("line{n}\n")).collect::<String>(),
        );

        let output = ReadFile::new(4096)
            .invoke(
                ctx(dir.path()),
                serde_json::json!({ "path": "many.txt", "offset": 4, "limit": 2 }),
            )
            .await
            .unwrap();

        assert!(output.content.contains("4\tline4"));
        assert!(output.content.contains("5\tline5"));
        assert!(!output.content.contains("line6"));
        assert!(output.truncated);
        assert!(output.content.contains("showed lines 4-5 of 10"));
    }

    #[tokio::test]
    async fn refuses_files_over_the_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "big.txt", &"x".repeat(64));

        let err = ReadFile::new(16)
            .invoke(ctx(dir.path()), serde_json::json!({ "path": "big.txt" }))
            .await
            .unwrap_err();

        assert!(matches!(err, Error::Tool { .. }), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn refuses_non_utf8_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();

        let err = ReadFile::new(1024)
            .invoke(ctx(dir.path()), serde_json::json!({ "path": "bin.dat" }))
            .await
            .unwrap_err();

        assert!(matches!(err, Error::Tool { .. }), "unexpected error: {err}");
    }

    #[test]
    fn schema_advertises_the_expected_arguments() {
        let schema = ReadFile::new(1024).schema();
        let properties = &schema["properties"];
        assert!(properties.get("path").is_some());
        assert!(properties.get("offset").is_some());
        assert!(properties.get("limit").is_some());
        assert_eq!(schema["required"][0], "path");
    }
}
