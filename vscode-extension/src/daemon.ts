import * as vscode from 'vscode';
import * as cp from 'child_process';
import * as path from 'path';
import * as fs from 'fs';
import { resolveBinaryPath, ensureExecutable } from './platform';


function stripAnsi(s: string): string {
    const escape = String.fromCharCode(27);
    const bell = String.fromCharCode(7);
    const pattern = new RegExp(`${escape}\\[[0-9;]*[A-Za-z]|${escape}\\][^${bell}]*${bell}`, 'g');
    return s.replace(pattern, '');
}

export interface DaemonOptions {
    extensionPath: string;
    workspaceFolder: string;
    outputChannel: vscode.OutputChannel;
    overrideBinaryPath?: string;
}

export class DaemonManager {
    private process: cp.ChildProcess | null = null;
    private binaryPath: string;
    private crosslinkDir: string;
    private outputChannel: vscode.OutputChannel;
    private isShuttingDown = false;

    constructor(private options: DaemonOptions) {
        this.binaryPath = resolveBinaryPath(
            options.extensionPath,
            options.overrideBinaryPath
        );
        this.crosslinkDir = path.join(options.workspaceFolder, '.crosslink');
        this.outputChannel = options.outputChannel;
    }




    public hasCrosslinkProject(): boolean {
        return fs.existsSync(this.crosslinkDir);
    }





    public async start(): Promise<void> {
        if (this.process && !this.process.killed) {
            this.outputChannel.appendLine('Daemon is already running');
            return;
        }

        if (!this.hasCrosslinkProject()) {
            throw new Error(
                `No .crosslink directory found in ${this.options.workspaceFolder}. ` +
                'Run "crosslink init" first.'
            );
        }


        ensureExecutable(this.binaryPath);

        this.outputChannel.appendLine(`Starting daemon: ${this.binaryPath}`);
        this.outputChannel.appendLine(`Crosslink dir: ${this.crosslinkDir}`);

        this.isShuttingDown = false;



        this.process = cp.spawn(this.binaryPath, ['daemon', 'run', '--dir', this.crosslinkDir], {
            stdio: ['pipe', 'pipe', 'pipe'],
            detached: false,
            windowsHide: true,
        });


        this.process.stdout?.on('data', (data: Buffer) => {
            const lines = data.toString().trim().split('\n');
            for (const line of lines) {
                this.outputChannel.appendLine(`[daemon] ${stripAnsi(line)}`);
            }
        });


        this.process.stderr?.on('data', (data: Buffer) => {
            const lines = data.toString().trim().split('\n');
            for (const line of lines) {
                this.outputChannel.appendLine(`[daemon:err] ${stripAnsi(line)}`);
            }
        });


        this.process.on('exit', (code, signal) => {
            if (!this.isShuttingDown) {
                this.outputChannel.appendLine(
                    `Daemon exited unexpectedly (code: ${code}, signal: ${signal})`
                );
            } else {
                this.outputChannel.appendLine(`Daemon stopped (code: ${code})`);
            }
            this.process = null;
        });


        this.process.on('error', (err) => {
            this.outputChannel.appendLine(`Daemon error: ${err.message}`);
            vscode.window.showErrorMessage(`Crosslink daemon error: ${err.message}`);
            this.process = null;
        });


        await new Promise<void>((resolve, reject) => {
            const timeout = setTimeout(() => {
                if (this.process && !this.process.killed) {
                    this.outputChannel.appendLine('Daemon started successfully');
                    resolve();
                } else {
                    reject(new Error('Daemon failed to start'));
                }
            }, 500);

            this.process?.on('error', (err) => {
                clearTimeout(timeout);
                reject(err);
            });
        });
    }




    public stop(): void {
        if (!this.process) {
            this.outputChannel.appendLine('Daemon is not running');
            return;
        }

        this.isShuttingDown = true;
        this.outputChannel.appendLine('Stopping daemon...');


        this.process.stdin?.end();

        const pid = this.process.pid;


        const killTimeout = setTimeout(() => {
            if (this.process && !this.process.killed) {
                this.outputChannel.appendLine('Daemon did not exit gracefully, forcing kill');
                if (process.platform === 'win32' && pid !== undefined) {

                    try {
                        cp.execFileSync('taskkill', ['/PID', String(pid), '/F']);
                    } catch (error) {
                        void error;
                    }
                } else {
                    this.process.kill('SIGKILL');
                }
            }
        }, 2000);

        this.process.on('exit', () => {
            clearTimeout(killTimeout);
        });


        if (process.platform === 'win32' && pid !== undefined) {

            try {
                cp.execFileSync('taskkill', ['/PID', String(pid)]);
            } catch (error) {
                void error;
            }
        } else {
            this.process.kill('SIGTERM');
        }
    }




    public isRunning(): boolean {
        return this.process !== null && !this.process.killed;
    }




    public getPid(): number | undefined {
        return this.process?.pid;
    }




    public async executeCommand(args: string[]): Promise<string> {
        ensureExecutable(this.binaryPath);

        return new Promise((resolve, reject) => {
            const proc = cp.spawn(this.binaryPath, args, {
                cwd: this.options.workspaceFolder,
                stdio: ['pipe', 'pipe', 'pipe'],
                windowsHide: true,
            });

            let stdout = '';
            let stderr = '';

            proc.stdout?.on('data', (data: Buffer) => {
                stdout += data.toString();
            });

            proc.stderr?.on('data', (data: Buffer) => {
                stderr += data.toString();
            });

            proc.on('exit', (code) => {
                if (code === 0) {
                    resolve(stdout.trim());
                } else {
                    reject(new Error(stderr.trim() || `Command failed with code ${code}`));
                }
            });

            proc.on('error', (err) => {
                reject(err);
            });
        });
    }




    public dispose(): void {
        this.stop();
    }
}
