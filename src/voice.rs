//! Moonshine Voice sidecar: local microphone input and spoken output.

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
};

const BRIDGE: &str = r#"
import json
import sys

from moonshine_voice import MicTranscriber, TextToSpeech, TranscriptEventListener, get_model_for_language


class Listener(TranscriptEventListener):
    def __init__(self):
        self.accepting = True

    def on_line_completed(self, event):
        text = event.line.text.strip()
        if self.accepting and text:
            self.accepting = False
            print(json.dumps({"text": text}), flush=True)


def main():
    model_path, model_arch = get_model_for_language("en")
    mic = MicTranscriber(model_path=model_path, model_arch=model_arch)
    listener = Listener()
    mic.add_listener(listener)
    tts = TextToSpeech("en_us")
    mic.start()
    print("voice ready — speak when ready; Ctrl+C quits", file=sys.stderr, flush=True)

    try:
        for line in sys.stdin:
            text = json.loads(line).get("speak", "").strip()
            if not text:
                continue
            mic.stop()
            try:
                tts.say(text)
                tts.wait()
            finally:
                listener.accepting = True
                mic.start()
    finally:
        mic.stop()
        mic.close()
        tts.close()


if __name__ == "__main__":
    main()
"#;

#[derive(Deserialize)]
struct VoicePrompt {
    text: String,
}

#[derive(Serialize)]
struct Speak<'a> {
    speak: &'a str,
}

pub(crate) struct VoiceBridge {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

impl VoiceBridge {
    pub(crate) fn start() -> Result<Self, String> {
        let mut child = Command::new("python3")
            .args(["-u", "-c", BRIDGE])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|error| format!("could not start Moonshine Voice (need python3): {error}"))?;
        let stdin = child.stdin.take().ok_or("Moonshine Voice stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Moonshine Voice stdout unavailable")?;
        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
        })
    }

    pub(crate) async fn next_prompt(&mut self) -> Result<Option<String>, String> {
        let Some(line) = self
            .lines
            .next_line()
            .await
            .map_err(|error| format!("Moonshine Voice output error: {error}"))?
        else {
            return Ok(None);
        };
        serde_json::from_str::<VoicePrompt>(&line)
            .map(|event| Some(event.text.trim().to_string()))
            .map_err(|error| format!("invalid Moonshine Voice message: {error}"))
    }

    pub(crate) async fn speak(&mut self, text: &str) -> Result<(), String> {
        let message = serde_json::to_string(&Speak { speak: text })
            .map_err(|error| format!("could not encode spoken reply: {error}"))?;
        self.stdin
            .write_all(message.as_bytes())
            .await
            .map_err(|error| format!("Moonshine Voice stopped: {error}"))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|error| format!("Moonshine Voice stopped: {error}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| format!("Moonshine Voice stopped: {error}"))
    }

    pub(crate) async fn stop(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }

    pub(crate) async fn wait(&mut self) -> Result<(), String> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|error| format!("could not wait for Moonshine Voice: {error}"))?;
        status
            .success()
            .then_some(())
            .ok_or_else(|| format!("Moonshine Voice exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::VoicePrompt;

    #[test]
    fn voice_prompt_preserves_spoken_text() {
        let prompt: VoicePrompt = serde_json::from_str(r#"{"text":"hello, Nano"}"#).unwrap();
        assert_eq!(prompt.text, "hello, Nano");
    }
}
