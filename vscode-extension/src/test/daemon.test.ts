import * as assert from 'assert';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import {
    CommandRunner,
    DaemonManager,
    DaemonReadinessResponse,
    parseDaemonResponse,
    readinessPresentation,
    reconnectAfterConfigurationChange,
    startSessionAfterReadiness,
} from '../daemon';

function response(
    state: DaemonReadinessResponse['state'],
    ready: boolean,
    running = true,
): DaemonReadinessResponse {
    return {
        schema_version: 1,
        protocol_version: 1,
        state,
        ready,
        running,
        repository_id: 'repo',
        daemon_epoch: 'epoch',
        daemon_pid: 42,
        attempt_id: 'attempt',
        generation_id: ready ? 'generation' : null,
        updated_at: '2026-08-31T00:00:00Z',
        reason: ready ? null : state,
        evidence_path: ready ? null : '/tmp/evidence',
        evidence_sha256: ready ? null : '0000000000000000000000000000000000000000000000000000000000000000',
    };
}

function stopped(reason: string | null = null): DaemonReadinessResponse {
    return {
        schema_version: 1,
        protocol_version: 1,
        state: null,
        ready: false,
        running: false,
        repository_id: null,
        daemon_epoch: null,
        daemon_pid: null,
        attempt_id: null,
        generation_id: null,
        updated_at: null,
        reason,
        evidence_path: null,
        evidence_sha256: null,
    };
}

function fixture(runner: CommandRunner): { manager: DaemonManager; dispose: () => void } {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'crosslink-vscode-readiness-'));
    fs.mkdirSync(path.join(root, '.crosslink'));
    fs.writeFileSync(path.join(root, '.crosslink', 'hook-config.json'), '{}');
    const binary = path.join(root, process.platform === 'win32' ? 'crosslink.exe' : 'crosslink');
    fs.writeFileSync(binary, '');
    const output = { appendLine: () => undefined };
    const manager = new DaemonManager({
        extensionPath: root,
        workspaceFolder: root,
        outputChannel: output as never,
        overrideBinaryPath: binary,
        commandRunner: runner,
    });
    return { manager, dispose: () => fs.rmSync(root, { recursive: true, force: true }) };
}

suite('daemon readiness', () => {
    test('stray crosslink directory is not an initialized project', async () => {
        const root = fs.mkdtempSync(path.join(os.tmpdir(), 'crosslink-vscode-stray-'));
        fs.mkdirSync(path.join(root, '.crosslink'));
        const binary = path.join(root, process.platform === 'win32' ? 'crosslink.exe' : 'crosslink');
        fs.writeFileSync(binary, '');
        const manager = new DaemonManager({
            extensionPath: root,
            workspaceFolder: root,
            outputChannel: { appendLine: () => undefined } as never,
            overrideBinaryPath: binary,
            commandRunner: async () => ({ code: 0, stdout: '', stderr: '' }),
        });
        try {
            assert.strictEqual(manager.hasCrosslinkProject(), false);
            await assert.rejects(manager.start(), /No initialized .crosslink project/);
        } finally {
            fs.rmSync(root, { recursive: true, force: true });
        }
    });

    test('parses stable readiness envelope', () => {
        for (const [state, ready] of [
            ['ready_current', true],
            ['ready_migrated', true],
            ['ready_adopted', true],
            ['waiting_for_remote', false],
            ['blocked_corrupt', false],
        ] as const) {
            assert.deepStrictEqual(
                parseDaemonResponse(JSON.stringify(response(state, ready))),
                response(state, ready),
            );
        }
        assert.deepStrictEqual(parseDaemonResponse(JSON.stringify(stopped())), stopped());
        assert.deepStrictEqual(
            parseDaemonResponse(JSON.stringify(stopped('daemon status failed'))),
            stopped('daemon status failed'),
        );
        const pending = response(null, false);
        assert.deepStrictEqual(parseDaemonResponse(JSON.stringify(pending)), pending);
    });

    test('presents ready, waiting, blocked, stopped, and failed readiness distinctly', () => {
        const ready = readinessPresentation(response('ready_current', true));
        assert.strictEqual(ready.tone, 'ready');
        assert.match(ready.tooltip, /ready_current/);
        const waiting = readinessPresentation(response('waiting_for_remote', false));
        assert.strictEqual(waiting.tone, 'waiting');
        assert.match(waiting.tooltip, /live.*waiting_for_remote/);
        const blocked = readinessPresentation(response('blocked_corrupt', false));
        assert.strictEqual(blocked.tone, 'blocked');
        assert.match(blocked.tooltip, /blocked_corrupt/);
        const stopped = readinessPresentation(null);
        assert.strictEqual(stopped.tone, 'stopped');
        const failed = readinessPresentation(null, 'malformed response');
        assert.strictEqual(failed.tone, 'waiting');
        assert.match(failed.tooltip, /malformed response/);
        const pending = readinessPresentation(response(null, false));
        assert.strictEqual(pending.tone, 'waiting');
        assert.match(pending.tooltip, /pending/);
    });

    test('rejects malformed readiness JSON', () => {
        assert.throws(() => parseDaemonResponse('{'), /malformed readiness JSON/);
    });

    test('rejects missing envelope fields', () => {
        const value = response('ready_current', true) as unknown as Record<string, unknown>;
        delete value.attempt_id;
        assert.throws(() => parseDaemonResponse(JSON.stringify(value)), /unsupported readiness envelope/);
    });

    test('rejects unknown envelope fields', () => {
        const value = response('ready_current', true) as unknown as Record<string, unknown>;
        value.unexpected = true;
        assert.throws(() => parseDaemonResponse(JSON.stringify(value)), /unsupported readiness envelope/);
    });

    test('rejects wrong envelope field types', () => {
        const value = response('ready_current', true) as unknown as Record<string, unknown>;
        value.daemon_pid = '42';
        assert.throws(() => parseDaemonResponse(JSON.stringify(value)), /unsupported readiness envelope/);
    });

    test('rejects unsupported protocol versions', () => {
        const value = response('ready_current', true) as unknown as Record<string, unknown>;
        value.protocol_version = 2;
        assert.throws(() => parseDaemonResponse(JSON.stringify(value)), /unsupported readiness envelope/);
    });

    test('rejects readiness state invariant violations', () => {
        const value = response('waiting_for_remote', true);
        assert.throws(() => parseDaemonResponse(JSON.stringify(value)), /state invariants/);
        const invalidEvidence = response('blocked_corrupt', false);
        invalidEvidence.evidence_sha256 = 'ABC';
        assert.throws(
            () => parseDaemonResponse(JSON.stringify(invalidEvidence)),
            /state invariants/,
        );
        const retainedIdentity = stopped();
        retainedIdentity.daemon_pid = 42;
        assert.throws(
            () => parseDaemonResponse(JSON.stringify(retainedIdentity)),
            /state invariants/,
        );
    });

    for (const state of ['waiting_for_remote', 'blocked_corrupt'] as const) {
        test(`start rejects ${state}`, async () => {
            const item = fixture(async () => ({
                code: state === 'waiting_for_remote' ? 20 : 21,
                stdout: JSON.stringify(response(state, false)),
                stderr: '',
            }));
            try {
                await assert.rejects(item.manager.start(), new RegExp(state));
                const presentation = readinessPresentation(item.manager.getLastResponse());
                assert.strictEqual(
                    presentation.tone,
                    state === 'blocked_corrupt' ? 'blocked' : 'waiting',
                );
                assert.match(presentation.tooltip, new RegExp(state));
            } finally {
                item.dispose();
            }
        });
    }

    test('start joins an existing ready daemon through ensure', async () => {
        let seenArgs: string[] = [];
        const item = fixture(async (_binary, args) => {
            seenArgs = args;
            return {
                code: 0,
                stdout: JSON.stringify(response('ready_current', true)),
                stderr: '',
            };
        });
        try {
            const result = await item.manager.start();
            assert.strictEqual(result.ready, true);
            assert.deepStrictEqual(seenArgs, ['daemon', 'ensure', '--wait-ready', '--json']);
        } finally {
            item.dispose();
        }
    });

    test('start propagates a bounded command timeout', async () => {
        const item = fixture(async () => {
            throw new Error('Crosslink command timed out after 131000ms');
        });
        try {
            await assert.rejects(item.manager.start(), /timed out/);
        } finally {
            item.dispose();
        }
    });

    test('status addresses the repository daemon through CLI', async () => {
        let seenArgs: string[] = [];
        const item = fixture(async (_binary, args) => {
            seenArgs = args;
            return {
                code: 0,
                stdout: JSON.stringify(response('ready_adopted', true)),
                stderr: '',
            };
        });
        try {
            const result = await item.manager.status();
            assert.strictEqual(result.state, 'ready_adopted');
            assert.deepStrictEqual(seenArgs, ['daemon', 'status', '--json']);
        } finally {
            item.dispose();
        }
    });

    test('session start ensures readiness before mutation', async () => {
        const calls: string[] = [];
        const output = await startSessionAfterReadiness({
            start: async () => {
                calls.push('ensure');
                return response('ready_current', true);
            },
            executeCommand: async args => {
                calls.push(args.join(' '));
                return 'started';
            },
        });
        assert.strictEqual(output, 'started');
        assert.deepStrictEqual(calls, ['ensure', 'session start']);
    });

    for (const state of ['waiting_for_remote', 'blocked_corrupt', 'error'] as const) {
        test(`session start remains blocked when readiness is ${state}`, async () => {
            const calls: string[] = [];
            await assert.rejects(
                startSessionAfterReadiness({
                    start: async () => {
                        calls.push('ensure');
                        throw new Error(state);
                    },
                    executeCommand: async args => {
                        calls.push(args.join(' '));
                        return 'unexpected';
                    },
                }),
                new RegExp(state),
            );
            assert.deepStrictEqual(calls, ['ensure']);
        });
    }

    test('configuration reconnect runs repository ensure and preserves non-ready failure', async () => {
        let calls = 0;
        const ready = await reconnectAfterConfigurationChange({
            start: async () => {
                calls += 1;
                return response('ready_adopted', true);
            },
        });
        assert.strictEqual(ready.state, 'ready_adopted');
        await assert.rejects(
            reconnectAfterConfigurationChange({
                start: async () => {
                    calls += 1;
                    throw new Error('waiting_for_remote');
                },
            }),
            /waiting_for_remote/,
        );
        assert.strictEqual(calls, 2);
    });
});
