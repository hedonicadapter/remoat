// Mic capture for local STT: hand raw PCM back to the main thread, which ships it to the
// daemon over the existing WebSocket.
//
// An AudioWorklet rather than MediaRecorder because iOS Safari's MediaRecorder emits mp4/AAC,
// not webm/opus — that would put an audio decoder on the daemon for no benefit. Raw 16-bit
// mono at 48 kHz is ~96 KB/s, which is nothing on a LAN.
//
// Int16 conversion happens here, in the audio thread, so the main thread never touches the
// samples: it just forwards an ArrayBuffer straight into ws.send().
class PcmWorklet extends AudioWorkletProcessor {
  constructor() {
    super();
    // Batch ~100ms per message. Posting every 128-frame render quantum would be ~375
    // messages/sec, each becoming its own WebSocket frame.
    this.target = Math.round(sampleRate / 10);
    this.buf = new Int16Array(this.target);
    this.n = 0;
  }

  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    if (!ch) return true;   // no input yet; keep the node alive
    for (let i = 0; i < ch.length; i++) {
      // Clamp before scaling: values outside [-1,1) are legal in Web Audio and would wrap.
      const s = Math.max(-1, Math.min(1, ch[i]));
      this.buf[this.n++] = s < 0 ? s * 0x8000 : s * 0x7fff;
      if (this.n === this.target) {
        // Transfer, don't copy — then allocate a fresh buffer since this one is now detached.
        this.port.postMessage(this.buf.buffer, [this.buf.buffer]);
        this.buf = new Int16Array(this.target);
        this.n = 0;
      }
    }
    return true;
  }
}

registerProcessor("pcm-worklet", PcmWorklet);
