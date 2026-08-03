import assert from "node:assert/strict";
import test from "node:test";
import { TerminalInputQueue, forwardDroppedInput } from "../src/terminalInput.ts";

const decode = (chunks: Uint8Array[]) => new TextDecoder().decode(
  chunks.reduce((all, chunk) => {
    const joined = new Uint8Array(all.length + chunk.length);
    joined.set(all);
    joined.set(chunk, all.length);
    return joined;
  }, new Uint8Array()),
);

test("keeps rapid keys, controls, and unicode in exact order", () => {
  const queue = new TerminalInputQueue();
  for (const input of ["p", "w", "d", "\t", "你", "好", "\r", "\u0003"]) queue.enqueue(input);

  const sent: Uint8Array[] = [];
  queue.flush((bytes) => sent.push(bytes));

  assert.equal(decode(sent), "pwd\t你好\r\u0003");
  assert.equal(queue.length, 0);
});

test("retains the failed item and all following input for retry", () => {
  const queue = new TerminalInputQueue();
  for (const input of ["a", "b", "c", "\r"]) queue.enqueue(input);

  const sent: Uint8Array[] = [];
  assert.throws(() => queue.flush((bytes) => {
    if (sent.length === 2) throw new Error("socket changed state");
    sent.push(bytes);
  }));
  assert.equal(decode(sent), "ab");
  assert.equal(queue.length, 2);

  queue.flush((bytes) => sent.push(bytes));
  assert.equal(decode(sent), "abc\r");
  assert.equal(queue.length, 0);
});

// --- forwardDroppedInput: recover the WKWebView keyCode-229 dropped character ---

const beforeInput = (inputType: string, data: string | null) => {
  let prevented = 0;
  const ev = { inputType, data, preventDefault: () => { prevented++; } };
  const sent: string[] = [];
  const claimed = forwardDroppedInput(ev, (d) => sent.push(d));
  return { claimed, sent, prevented: () => prevented };
};

test("forwards a dropped insertText character and cancels the DOM mutation", () => {
  const r = beforeInput("insertText", "w");
  assert.equal(r.claimed, true);
  assert.deepEqual(r.sent, ["w"]);
  // preventDefault suppresses xterm's own `input` handler, so there is no double.
  assert.equal(r.prevented(), 1);
});

test("leaves composition, paste and deletion to xterm untouched", () => {
  for (const inputType of [
    "insertCompositionText", // IME in progress
    "insertFromComposition", // IME commit
    "insertFromPaste",       // clipboard
    "deleteContentBackward", // Backspace
    "insertLineBreak",       // Enter in some engines
  ]) {
    const r = beforeInput(inputType, "x");
    assert.equal(r.claimed, false, `${inputType} must stay with xterm`);
    assert.deepEqual(r.sent, []);
    assert.equal(r.prevented(), 0, `${inputType} must not be cancelled`);
  }
});

test("ignores empty insertText so no stray byte is sent", () => {
  const r = beforeInput("insertText", null);
  assert.equal(r.claimed, false);
  assert.deepEqual(r.sent, []);
  assert.equal(r.prevented(), 0);
});

test("recovered characters keep typing order through the FIFO", () => {
  // The reported symptom: "pwd" typed fast where only the middle key takes
  // WebKit's keyCode-229 path. 'p' and 'd' arrive via onData; 'w' would be lost
  // but is recovered by forwardDroppedInput. All three share one queue, so the
  // PTY receives "pwd", not "pd".
  const queue = new TerminalInputQueue();
  const forward = (d: string) => queue.enqueue(d);
  queue.enqueue("p");                                    // onData
  forwardDroppedInput({ inputType: "insertText", data: "w", preventDefault() {} }, forward);
  queue.enqueue("d");                                    // onData

  const sent: Uint8Array[] = [];
  queue.flush((bytes) => sent.push(bytes));
  assert.equal(decode(sent), "pwd");
});
