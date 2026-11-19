import assert from "node:assert/strict";
import { test } from "node:test";
import { AFTER_COMMAND_MILLIS, afterSystemCommand, onSystemCommand } from "../src/systemCommand.ts";

test("after a command returns, every listener is told at once and again 1.5 seconds later", async (context) => {
  context.mock.timers.enable({ apis: ["setTimeout"] });
  let told = 0;
  const stop = onSystemCommand(() => (told += 1));
  const result = await afterSystemCommand(async () => "unmounted");
  assert.equal(result, "unmounted");
  assert.equal(told, 1, "told when the command returned");
  context.mock.timers.tick(AFTER_COMMAND_MILLIS - 1);
  assert.equal(told, 1, "not yet");
  context.mock.timers.tick(1);
  assert.equal(told, 2, "told again, because the mount table can lag");
  assert.equal(AFTER_COMMAND_MILLIS, 1500);
  stop();
});

test("a command that failed is read back too, and its failure still reaches the caller", async (context) => {
  context.mock.timers.enable({ apis: ["setTimeout"] });
  let told = 0;
  const stop = onSystemCommand(() => (told += 1));
  await assert.rejects(afterSystemCommand(async () => Promise.reject(new Error("macOS would not unmount"))), /would not unmount/);
  assert.equal(told, 1);
  context.mock.timers.tick(AFTER_COMMAND_MILLIS);
  assert.equal(told, 2);
  stop();
});

test("a listener that stopped is not told", async (context) => {
  context.mock.timers.enable({ apis: ["setTimeout"] });
  let told = 0;
  const stop = onSystemCommand(() => (told += 1));
  stop();
  await afterSystemCommand(async () => undefined);
  context.mock.timers.tick(AFTER_COMMAND_MILLIS);
  assert.equal(told, 0);
});
