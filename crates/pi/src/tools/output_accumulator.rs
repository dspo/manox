// Output accumulator — spill large bash output to temp files.
//
// When a shell command produces very large output (e.g. `cat large_file.log`),
// holding the entire output in memory can cause OOM. The accumulator writes
// output to a temp file in chunks, then provides a summary with the file path
// and a truncated preview.

use std::path::PathBuf;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Maximum bytes to keep in memory before spilling to disk.
const MAX_IN_MEMORY_BYTES: usize = 64 * 1024; // 64 KiB

/// An accumulator that collects output and spills to disk when it exceeds
/// the in-memory limit.
pub struct OutputAccumulator {
    /// In-memory buffer for small outputs.
    buffer: String,
    /// Path to the temp file, if the output was spilled.
    spill_path: Option<PathBuf>,
    /// Total bytes accumulated.
    total_bytes: usize,
    /// Whether the accumulator is sealed (no more writes).
    sealed: bool,
}

impl Default for OutputAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputAccumulator {
    /// Create a new output accumulator.
    pub fn new() -> Self {
        OutputAccumulator {
            buffer: String::new(),
            spill_path: None,
            total_bytes: 0,
            sealed: false,
        }
    }

    /// Append a chunk of output.
    pub async fn append(&mut self, chunk: &str) -> Result<(), std::io::Error> {
        if self.sealed {
            return Ok(());
        }

        self.total_bytes += chunk.len();

        if let Some(spill_path) = &self.spill_path {
            // Already spilling — append to the temp file.
            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(spill_path)
                .await?;
            file.write_all(chunk.as_bytes()).await?;
        } else if self.buffer.len() + chunk.len() > MAX_IN_MEMORY_BYTES {
            // Crossed the threshold — spill to disk.
            let dir = std::env::temp_dir();
            let spill_path = dir.join(format!("pi-output-{}.tmp", uuid::Uuid::new_v4()));
            let mut file = tokio::fs::File::create(&spill_path).await?;
            file.write_all(self.buffer.as_bytes()).await?;
            file.write_all(chunk.as_bytes()).await?;
            self.spill_path = Some(spill_path);
            self.buffer = String::new();
        } else {
            // Still fits in memory.
            self.buffer.push_str(chunk);
        }

        Ok(())
    }

    /// Seal the accumulator (no more writes) and return the final output.
    pub async fn seal(&mut self) -> Result<AccumulatedOutput, std::io::Error> {
        self.sealed = true;

        if let Some(ref spill_path) = self.spill_path {
            // Read back a preview from the spill file.
            let file = File::open(spill_path).await?;
            let reader = BufReader::new(file);
            let mut lines = reader.lines();
            let mut preview = String::new();
            let mut line_count = 0usize;
            while let Some(line) = lines.next_line().await? {
                if line_count < 20 {
                    if !preview.is_empty() {
                        preview.push('\n');
                    }
                    preview.push_str(&line);
                }
                line_count += 1;
            }

            let preview = if line_count > 20 {
                format!("{preview}\n... [{line_count} total lines]")
            } else {
                preview
            };

            Ok(AccumulatedOutput {
                content: format!(
                    "Output written to: {}\n\nPreview:\n{preview}",
                    spill_path.display()
                ),
                total_bytes: self.total_bytes,
                was_spilled: true,
                spill_path: Some(spill_path.clone()),
            })
        } else {
            Ok(AccumulatedOutput {
                content: self.buffer.clone(),
                total_bytes: self.total_bytes,
                was_spilled: false,
                spill_path: None,
            })
        }
    }

    /// Total bytes accumulated so far.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Whether the output has been spilled to disk.
    pub fn was_spilled(&self) -> bool {
        self.spill_path.is_some()
    }
}

/// The final accumulated output from a command.
#[derive(Debug, Clone)]
pub struct AccumulatedOutput {
    /// The content to send to the LLM (preview or full output).
    pub content: String,
    /// Total bytes of output.
    pub total_bytes: usize,
    /// Whether the output was spilled to a temp file.
    pub was_spilled: bool,
    /// Path to the temp file, if spilled.
    pub spill_path: Option<PathBuf>,
}

impl Drop for OutputAccumulator {
    fn drop(&mut self) {
        // Clean up the temp file if it exists.
        if let Some(ref path) = self.spill_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_small_output_stays_in_memory() {
        let mut acc = OutputAccumulator::new();
        acc.append("hello").await.unwrap();
        acc.append(" world").await.unwrap();

        let output = acc.seal().await.unwrap();
        assert!(!output.was_spilled);
        assert_eq!(output.content, "hello world");
        assert_eq!(output.total_bytes, 11);
    }

    #[tokio::test]
    async fn test_large_output_spills() {
        let mut acc = OutputAccumulator::new();
        let chunk = "x".repeat(MAX_IN_MEMORY_BYTES + 1);
        acc.append(&chunk).await.unwrap();

        let output = acc.seal().await.unwrap();
        assert!(output.was_spilled);
        assert!(output.content.contains("Output written to:"));
        assert!(output.content.contains("Preview"));
    }
}