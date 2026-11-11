//! Bounded subprocess execution: drain everything, retain the useful ends.
//!
//! Cut from the core's text module when the core stopped running processes.
//! Both callers that needed this got the same two subtle things right
//! independently — the write sits inside the timeout, and the child is killed
//! on drop — which is why it is one function and not a habit.

/// A process result whose pipes were drained without retaining unbounded data.
pub struct CommandOutput {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Keep the useful ends of a stream while continuing to drain all of it.
///
/// Stopping at the limit would let the child's pipe fill and deadlock it. A
/// head and tail are also more useful than either alone: diagnostics usually
/// introduce themselves at the start and explain the failure at the end.
#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    omitted: usize,
    limit: usize,
}

impl Capture {
    fn new(limit: usize) -> Self {
        Self { limit, ..Self::default() }
    }

    fn push(&mut self, chunk: &[u8]) {
        if self.bytes.len() + chunk.len() <= self.limit {
            self.bytes.extend_from_slice(chunk);
            return;
        }
        let head = self.limit / 2;
        let tail = self.limit - head;
        let old_total = self.bytes.len() + self.omitted;
        let mut end = Vec::with_capacity(tail + chunk.len());
        if self.bytes.len() > head {
            end.extend_from_slice(&self.bytes[head..]);
        }
        end.extend_from_slice(chunk);
        if self.bytes.len() > head {
            self.bytes.truncate(head);
        }
        if self.bytes.len() < head {
            let take = (head - self.bytes.len()).min(chunk.len());
            self.bytes.extend_from_slice(&chunk[..take]);
        }
        let keep = end.len().min(tail);
        self.bytes.extend_from_slice(&end[end.len() - keep..]);
        self.omitted = old_total + chunk.len() - self.bytes.len();
    }

    fn render(self) -> String {
        if self.omitted == 0 {
            return String::from_utf8_lossy(&self.bytes).into_owned();
        }
        let head = self.limit / 2;
        let split = head.min(self.bytes.len());
        format!(
            "{}\n[output truncated; {} bytes omitted]\n{}",
            String::from_utf8_lossy(&self.bytes[..split]),
            self.omitted,
            String::from_utf8_lossy(&self.bytes[split..])
        )
    }
}

async fn capture(mut pipe: impl tokio::io::AsyncRead + Unpin, limit: usize) -> std::io::Result<String> {
    use tokio::io::AsyncReadExt;
    let mut capture = Capture::new(limit);
    let mut chunk = [0_u8; 8192];
    loop {
        let bytes = pipe.read(&mut chunk).await?;
        if bytes == 0 {
            return Ok(capture.render());
        }
        capture.push(&chunk[..bytes]);
    }
}

/// Run `command`, hand it `input`, and take what it wrote — or give up.
///
/// Written once because both callers that needed it got the same two subtle
/// things right independently, and a third would have had to get them right
/// again: the write sits *inside* the bound, since a program that never reads
/// its input would otherwise block forever once the pipe fills, and the child
/// is killed on drop, so giving up actually ends it.
pub async fn feed(
    mut command: tokio::process::Command,
    input: &[u8],
    limit: std::time::Duration,
    what: &str,
) -> elmee_core::Result<CommandOutput> {
    run(&mut command, Some(input), limit, what, 48 * 1024).await
}

/// Run a command while bounding retained output, not how much is drained.
pub async fn command(
    command: &mut tokio::process::Command,
    limit: std::time::Duration,
    what: &str,
) -> elmee_core::Result<CommandOutput> {
    run(command, None, limit, what, 48 * 1024).await
}

async fn run(
    command: &mut tokio::process::Command,
    input: Option<&[u8]>,
    limit: std::time::Duration,
    what: &str,
    output_limit: usize,
) -> elmee_core::Result<CommandOutput> {
    use tokio::io::AsyncWriteExt;
    let mut child = crate::scope(
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
    )
    .spawn()?;
    // A timeout or a cancelled caller ends everything the command started,
    // not only the command.
    let group = crate::Group::of(&child);
    let stdout =
        child.stdout.take().ok_or_else(|| elmee_core::Error::Tool(format!("{what} has no stdout")))?;
    let stderr =
        child.stderr.take().ok_or_else(|| elmee_core::Error::Tool(format!("{what} has no stderr")))?;
    let stdin = child.stdin.take();
    tokio::time::timeout(limit, async {
        let write = async {
            if let Some(mut stdin) = stdin
                && let Some(input) = input
            {
                stdin.write_all(input).await?;
            }
            Ok::<_, std::io::Error>(())
        };
        let (written, stdout, stderr, status) =
            tokio::join!(write, capture(stdout, output_limit), capture(stderr, output_limit), child.wait());
        written?;
        Ok(CommandOutput { status: status?, stdout: stdout?, stderr: stderr? })
    })
    .await
    .map_err(|_| elmee_core::Error::Tool(format!("{what} timed out after {} seconds", limit.as_secs())))
    .and_then(|finished: elmee_core::Result<CommandOutput>| {
        // Ended on its own: what it left running in the background on purpose
        // is its business, as it is in any shell.
        group.release();
        finished
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capture_keeps_both_ends_and_counts_what_it_drops() {
        let mut capture = Capture::new(8);
        capture.push(b"012345");
        capture.push(b"6789abcdef");
        assert_eq!(capture.render(), "0123\n[output truncated; 8 bytes omitted]\ncdef");
    }
    #[test]
    fn capture_is_independent_of_stream_chunk_boundaries() {
        let input: Vec<u8> = (0..=255).cycle().take(8_000).collect();
        let render = |chunk: usize| {
            let mut capture = Capture::new(1_024);
            for part in input.chunks(chunk) {
                capture.push(part);
            }
            capture.render()
        };
        let expected = render(input.len());
        for chunk in 1..=257 {
            assert_eq!(render(chunk), expected, "chunk size {chunk}");
        }
    }
}
