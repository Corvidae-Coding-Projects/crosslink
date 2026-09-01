import * as vscode from 'vscode';
import * as cp from 'child_process';
import * as path from 'path';
import * as fs from 'fs';
import { resolveBinaryPath, ensureExecutable } from './platform';

export type ReadinessState =
    | 'ready_current'
    | 'ready_migrated'
    | 'ready_adopted'
    | 'waiting_for_remote'
    | 'blocked_corrupt';

export interface DaemonReadinessResponse {
    schema_version: number;
    protocol_version: number;
    state: ReadinessState | null;
    ready: boolean;
    running: boolean;
    repository_id: string | null;
    daemon_epoch: string | null;
    daemon_pid: number | null;
    attempt_id: string | null;
    generation_id: string | null;
    updated_at: string | null;
    reason: string | null;
    evidence_path: string | null;
    evidence_sha256: string | null;
}

export interface ReadinessPresentation {
    text: string;
    tooltip: string;
    tone: 'ready' | 'waiting' | 'blocked' | 'stopped';
}

export function readinessPresentation(
    response: DaemonReadinessResponse | null,
    error?: string,
): ReadinessPresentation {
    if (response?.ready) {
        return {
            text: '$(pulse) Crosslink',
            tooltip: `Crosslink repository ready (${response.state}, PID ${response.daemon_pid})`,
            tone: 'ready',
        };
    }
    if (response?.running) {
        const reason = response.reason ? `: ${response.reason}` : '';
        const blocked = response.state === 'blocked_corrupt';
        const state = response.state ?? 'pending';
        return {
            text: blocked ? '$(error) Crosslink' : '$(sync~spin) Crosslink',
            tooltip: `Crosslink daemon is live but repository readiness is ${state}${reason}`,
            tone: blocked ? 'blocked' : 'waiting',
        };
    }
    if (error) {
        return {
            text: '$(warning) Crosslink',
            tooltip: `Crosslink readiness unavailable: ${error}`,
            tone: 'waiting',
        };
    }
    return {
        text: '$(circle-slash) Crosslink',
        tooltip: 'Crosslink daemon stopped (click for status)',
        tone: 'stopped',
    };
}

export interface CommandResult {
    code: number | null;
    stdout: string;
    stderr: string;
}

export type CommandRunner = (
    binaryPath: string,
    args: string[],
    cwd: string,
    timeoutMs: number,
) => Promise<CommandResult>;

export interface DaemonOptions {
    extensionPath: string;
    workspaceFolder: string;
    outputChannel: vscode.OutputChannel;
    overrideBinaryPath?: string;
    commandRunner?: CommandRunner;
}

export interface ReadinessController {
    start(): Promise<DaemonReadinessResponse>;
    executeCommand(args: string[]): Promise<string>;
}

export async function startSessionAfterReadiness(
    controller: ReadinessController,
): Promise<string> {
    await controller.start();
    return controller.executeCommand(['session', 'start']);
}

export async function reconnectAfterConfigurationChange(
    controller: Pick<ReadinessController, 'start'>,
): Promise<DaemonReadinessResponse> {
    return controller.start();
}

const STATES = new Set<ReadinessState>([
    'ready_current',
    'ready_migrated',
    'ready_adopted',
    'waiting_for_remote',
    'blocked_corrupt',
]);

const RESPONSE_FIELDS = [
    'attempt_id',
    'daemon_epoch',
    'daemon_pid',
    'evidence_path',
    'evidence_sha256',
    'generation_id',
    'protocol_version',
    'ready',
    'reason',
    'repository_id',
    'running',
    'schema_version',
    'state',
    'updated_at',
];

export function parseDaemonResponse(output: string): DaemonReadinessResponse {
    let value: unknown;
    try {
        value = JSON.parse(output.trim());
    } catch {
        throw new Error('Crosslink daemon returned malformed readiness JSON');
    }
    if (!value || typeof value !== 'object') {
        throw new Error('Crosslink daemon returned an invalid readiness envelope');
    }
    const response = value as Record<string, unknown>;
    const fields = Object.keys(response).sort();
    const nullableString = (field: string) =>
        response[field] === null || typeof response[field] === 'string';
    const nullablePid = response.daemon_pid === null
        || (typeof response.daemon_pid === 'number'
            && Number.isInteger(response.daemon_pid)
            && response.daemon_pid > 0);
    if (
        response.schema_version !== 1
        || response.protocol_version !== 1
        || fields.length !== RESPONSE_FIELDS.length
        || fields.some((field, index) => field !== RESPONSE_FIELDS[index])
        || (response.state !== null && (
            typeof response.state !== 'string'
            || !STATES.has(response.state as ReadinessState)
        ))
        || typeof response.ready !== 'boolean'
        || typeof response.running !== 'boolean'
        || !nullableString('repository_id')
        || !nullableString('daemon_epoch')
        || !nullablePid
        || !nullableString('attempt_id')
        || !nullableString('generation_id')
        || !nullableString('updated_at')
        || !nullableString('reason')
        || !nullableString('evidence_path')
        || !nullableString('evidence_sha256')
    ) {
        throw new Error('Crosslink daemon returned an unsupported readiness envelope');
    }
    const readyState = response.state === 'ready_current'
        || response.state === 'ready_migrated'
        || response.state === 'ready_adopted';
    const activeState = response.running === true;
    const evidenceState = response.state === 'waiting_for_remote'
        || response.state === 'blocked_corrupt';
    const evidenceDigest = response.evidence_sha256;
    if (
        response.ready !== readyState
        || (response.state !== null && response.running !== true)
        || (activeState && (
            typeof response.repository_id !== 'string'
            || response.repository_id.length === 0
            || typeof response.daemon_epoch !== 'string'
            || response.daemon_epoch.length === 0
            || typeof response.daemon_pid !== 'number'
            || typeof response.attempt_id !== 'string'
            || response.attempt_id.length === 0
            || typeof response.updated_at !== 'string'
            || response.updated_at.length === 0
        ))
        || (readyState && (
            typeof response.generation_id !== 'string'
            || response.generation_id.length === 0
        ))
        || (evidenceState && (
            typeof response.reason !== 'string'
            || response.reason.length === 0
            || typeof response.evidence_path !== 'string'
            || response.evidence_path.length === 0
            || typeof evidenceDigest !== 'string'
            || !/^[0-9a-f]{64}$/.test(evidenceDigest)
        ))
        || ((response.evidence_path === null) !== (response.evidence_sha256 === null))
        || (typeof evidenceDigest === 'string' && !/^[0-9a-f]{64}$/.test(evidenceDigest))
        || (!activeState && (
            response.ready !== false
            || response.repository_id !== null
            || response.daemon_epoch !== null
            || response.daemon_pid !== null
            || response.attempt_id !== null
            || response.generation_id !== null
            || response.updated_at !== null
            || response.evidence_path !== null
            || response.evidence_sha256 !== null
        ))
    ) {
        throw new Error('Crosslink daemon readiness envelope violates state invariants');
    }
    return response as unknown as DaemonReadinessResponse;
}

function runCommand(
    binaryPath: string,
    args: string[],
    cwd: string,
    timeoutMs: number,
): Promise<CommandResult> {
    return new Promise((resolve, reject) => {
        const child = cp.spawn(binaryPath, args, {
            cwd,
            stdio: ['ignore', 'pipe', 'pipe'],
            windowsHide: true,
        });
        let stdout = '';
        let stderr = '';
        let settled = false;
        const timer = timeoutMs > 0 ? setTimeout(() => {
            if (settled) {
                return;
            }
            settled = true;
            child.kill();
            reject(new Error(`Crosslink command timed out after ${timeoutMs}ms`));
        }, timeoutMs) : undefined;
        child.stdout?.on('data', (data: Buffer) => {
            stdout += data.toString();
        });
        child.stderr?.on('data', (data: Buffer) => {
            stderr += data.toString();
        });
        child.on('error', error => {
            if (settled) {
                return;
            }
            settled = true;
            if (timer) {
                clearTimeout(timer);
            }
            reject(error);
        });
        child.on('exit', code => {
            if (settled) {
                return;
            }
            settled = true;
            if (timer) {
                clearTimeout(timer);
            }
            resolve({ code, stdout: stdout.trim(), stderr: stderr.trim() });
        });
    });
}

export class DaemonManager {
    private readonly binaryPath: string;
    private readonly crosslinkDir: string;
    private readonly outputChannel: vscode.OutputChannel;
    private readonly commandRunner: CommandRunner;
    private lastResponse: DaemonReadinessResponse | null = null;

    constructor(private readonly options: DaemonOptions) {
        this.binaryPath = resolveBinaryPath(options.extensionPath, options.overrideBinaryPath);
        this.crosslinkDir = path.join(options.workspaceFolder, '.crosslink');
        this.outputChannel = options.outputChannel;
        this.commandRunner = options.commandRunner ?? runCommand;
    }

    public hasCrosslinkProject(): boolean {
        return fs.existsSync(path.join(this.crosslinkDir, 'hook-config.json'));
    }

    public async start(): Promise<DaemonReadinessResponse> {
        if (!this.hasCrosslinkProject()) {
            throw new Error(
                `No initialized .crosslink project found in ${this.options.workspaceFolder}. Run "crosslink init" first.`,
            );
        }
        ensureExecutable(this.binaryPath);
        this.outputChannel.appendLine(`Ensuring repository readiness: ${this.binaryPath}`);
        const result = await this.commandRunner(
            this.binaryPath,
            ['daemon', 'ensure', '--wait-ready', '--json'],
            this.options.workspaceFolder,
            131000,
        );
        const response = parseDaemonResponse(result.stdout);
        this.lastResponse = response;
        if (!response.ready || result.code !== 0) {
            throw new Error(
                `Repository readiness is ${response.state}${response.reason ? `: ${response.reason}` : ''}`,
            );
        }
        this.outputChannel.appendLine(
            `Repository ready (${response.state}, PID ${response.daemon_pid ?? 'unknown'})`,
        );
        return response;
    }

    public async stop(): Promise<void> {
        ensureExecutable(this.binaryPath);
        const result = await this.commandRunner(
            this.binaryPath,
            ['daemon', 'stop'],
            this.options.workspaceFolder,
            10000,
        );
        if (result.code !== 0) {
            throw new Error(result.stderr || `Crosslink daemon stop failed with code ${result.code}`);
        }
        this.lastResponse = null;
        this.outputChannel.appendLine('Repository daemon stopped');
    }

    public async status(): Promise<DaemonReadinessResponse> {
        ensureExecutable(this.binaryPath);
        const result = await this.commandRunner(
            this.binaryPath,
            ['daemon', 'status', '--json'],
            this.options.workspaceFolder,
            10000,
        );
        const response = parseDaemonResponse(result.stdout);
        this.lastResponse = response;
        if (result.code !== 0) {
            throw new Error(response.reason || result.stderr || `Daemon status failed with code ${result.code}`);
        }
        return response;
    }

    public isRunning(): boolean {
        return this.lastResponse?.running === true;
    }

    public getPid(): number | undefined {
        return this.lastResponse?.daemon_pid ?? undefined;
    }

    public getLastResponse(): DaemonReadinessResponse | null {
        return this.lastResponse;
    }

    public async executeCommand(args: string[]): Promise<string> {
        ensureExecutable(this.binaryPath);
        const result = await this.commandRunner(
            this.binaryPath,
            args,
            this.options.workspaceFolder,
            0,
        );
        if (result.code !== 0) {
            throw new Error(result.stderr || result.stdout || `Command failed with code ${result.code}`);
        }
        return result.stdout;
    }

    public dispose(): void {
        this.lastResponse = null;
    }
}
