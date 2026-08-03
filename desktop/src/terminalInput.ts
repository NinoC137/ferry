/** A lossless FIFO between xterm input events and WebSocket.send(). */
export class TerminalInputQueue {
  private readonly pending: Uint8Array[] = [];
  private readonly encoder: TextEncoder;

  constructor(encoder = new TextEncoder()) {
    this.encoder = encoder;
  }

  enqueue(data: string): void {
    this.pending.push(this.encoder.encode(data));
  }

  flush(send: (bytes: Uint8Array) => void): void {
    while (this.pending.length) {
      // Keep the head in place when send throws. The caller can retry after
      // the socket becomes writable again without losing or reordering input.
      send(this.pending[0]);
      this.pending.shift();
    }
  }

  get length(): number {
    return this.pending.length;
  }
}

/** The subset of a DOM `beforeinput` event this recovery needs. `InputEvent`
 *  satisfies it structurally, so callers pass the real event unchanged. */
interface EditableInputEvent {
  readonly inputType: string;
  readonly data: string | null;
  preventDefault(): void;
}

/**
 * Recover characters that xterm.js 6.x drops on macOS WKWebView.
 *
 * Under fast typing WebKit intermittently routes a keystroke through its
 * marked-text/insertText path, firing a `keydown` with `keyCode === 229`.
 * xterm's `_keyDown` sets `_keyDownSeen = true` but emits nothing
 * (`CompositionHelper.keydown` returns false for 229), and its `_inputEvent`
 * then refuses the follow-up `input` event because `(!ev.composed ||
 * !_keyDownSeen)` is false. Neither handler produces the byte, so it never
 * reaches `onData` — quickly typed "pwd" reaches the PTY as "pd".
 *
 * We claim the earlier `beforeinput` event for plain text insertion, forward
 * the data through the same lossless FIFO, and `preventDefault()` so the
 * textarea never mutates. Cancelling the mutation stops xterm's own `input`
 * listener from firing, so there is exactly one send and no double. Normal keys
 * are already cancelled in `_keyDown`, and the caps-lock A-Z hack in
 * `_keyPress`, so a `beforeinput`/`insertText` event fires only for characters
 * xterm would otherwise lose. Composition (IME), paste and deletion carry other
 * `inputType`s and are left to xterm.
 *
 * @returns whether the event was claimed (data forwarded and default cancelled).
 */
export function forwardDroppedInput(
  ev: EditableInputEvent,
  forward: (data: string) => void,
): boolean {
  if (ev.inputType !== "insertText" || !ev.data) return false;
  forward(ev.data);
  ev.preventDefault();
  return true;
}
