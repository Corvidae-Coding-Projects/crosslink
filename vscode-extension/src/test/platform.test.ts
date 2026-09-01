







import * as assert from "assert";
import * as path from "path";
import * as os from "os";
import * as fs from "fs";
import { detectPlatform, resolveBinaryPath } from "../platform";





suite("platform.ts — detectPlatform", () => {
  test("linux x64 — correct binary name and requiresChmod=true", () => {
    const info = detectPlatform("linux", "x64");
    assert.strictEqual(info.binaryName, "crosslink-linux");
    assert.strictEqual(info.requiresChmod, true);
  });

  test("linux arm64 — binary name has -arm64 suffix", () => {
    const info = detectPlatform("linux", "arm64");
    assert.strictEqual(info.binaryName, "crosslink-linux-arm64");
  });

  test("darwin x64 — correct binary name", () => {
    const info = detectPlatform("darwin", "x64");
    assert.strictEqual(info.binaryName, "crosslink-darwin");
    assert.strictEqual(info.requiresChmod, true);
  });

  test("win32 x64 — binary has .exe extension, requiresChmod=false", () => {
    const info = detectPlatform("win32", "x64");
    assert.strictEqual(info.binaryName, "crosslink-win.exe");
    assert.strictEqual(info.requiresChmod, false);
  });

  test("unsupported platform throws", () => {
    assert.throws(() => detectPlatform("freebsd", "x64"), /Unsupported platform/);
  });

  test("unsupported architecture throws", () => {
    assert.throws(() => detectPlatform("linux", "ia32"), /Unsupported architecture/);
  });
});

suite("platform.ts — resolveBinaryPath with override", () => {
  let testDir: string;

  setup(() => {
    testDir = fs.mkdtempSync(path.join(os.tmpdir(), "crosslink-platform-"));
  });
  teardown(() => {
    fs.rmSync(testDir, { recursive: true, force: true });
  });

  test("returns resolved path when override file exists", () => {
    const binaryPath = path.join(testDir, "crosslink");
    fs.writeFileSync(binaryPath, "");
    const result = resolveBinaryPath(testDir, binaryPath);
    assert.strictEqual(result, binaryPath);
  });

  test("throws when override file does not exist", () => {
    assert.throws(
      () => resolveBinaryPath(testDir, path.join(testDir, "missing")),
      /Configured binary not found/
    );
  });

  test("ignores empty-string override (uses bundled binary path logic)", () => {
    assert.throws(
      () => resolveBinaryPath(testDir, ""),
      /Bundled binary not found/
    );
  });
});
