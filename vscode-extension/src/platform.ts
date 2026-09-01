import * as os from 'os';
import * as path from 'path';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { chmodSync } from 'fs';

export type Platform = 'win32' | 'linux' | 'darwin';
export type Architecture = 'x64' | 'arm64';

export interface PlatformInfo {
    platform: Platform;
    arch: Architecture;
    binaryName: string;
    requiresChmod: boolean;
}

export function detectPlatform(
    platformValue: string = os.platform(),
    archValue: string = os.arch()
): PlatformInfo {
    const platform = platformValue as Platform;
    const arch = archValue as Architecture;


    if (!['win32', 'linux', 'darwin'].includes(platform)) {
        throw new Error(`Unsupported platform: ${platform}`);
    }


    if (!['x64', 'arm64'].includes(arch)) {
        throw new Error(`Unsupported architecture: ${arch}`);
    }


    const binaryName = getBinaryName(platform, arch);
    const requiresChmod = platform !== 'win32';

    return {
        platform,
        arch,
        binaryName,
        requiresChmod,
    };
}




function getBinaryName(platform: Platform, arch: Architecture): string {
    const archSuffix = arch === 'arm64' ? '-arm64' : '';

    switch (platform) {
        case 'win32':
            return `crosslink-win${archSuffix}.exe`;
        case 'linux':
            return `crosslink-linux${archSuffix}`;
        case 'darwin':
            return `crosslink-darwin${archSuffix}`;
        default:
            throw new Error(`Unknown platform: ${platform}`);
    }
}








export function resolveBinaryPath(extensionPath: string, overridePath?: string): string {

    if (overridePath && overridePath.trim() !== '') {
        const resolved = path.resolve(overridePath);
        if (!fs.existsSync(resolved)) {
            throw new Error(`Configured binary not found: ${resolved}`);
        }
        return resolved;
    }


    const platformInfo = detectPlatform();
    const binaryPath = path.join(extensionPath, 'bin', platformInfo.binaryName);

    if (!fs.existsSync(binaryPath)) {
        throw new Error(
            `Bundled binary not found: ${binaryPath}\n` +
            `Expected binary for ${platformInfo.platform}/${platformInfo.arch}`
        );
    }

    return binaryPath;
}







export function ensureExecutable(binaryPath: string): void {
    const platformInfo = detectPlatform();

    if (!platformInfo.requiresChmod) {

        return;
    }

    try {

        const stats = fs.statSync(binaryPath);
        const isExecutable = (stats.mode & fs.constants.S_IXUSR) !== 0;

        if (!isExecutable) {


            chmodSync(binaryPath, 0o755);
        }
    } catch (error) {
        throw new Error(
            `Failed to set executable permissions on ${binaryPath}: ${error}`
        );
    }
}







export function verifyBinaryChecksum(binaryPath: string): void {
    const checksumPath = binaryPath + '.sha256';
    if (!fs.existsSync(checksumPath)) {
        return;
    }
    const expected = fs.readFileSync(checksumPath, 'utf-8').trim();
    const hash = crypto.createHash('sha256');
    hash.update(fs.readFileSync(binaryPath));
    const actual = hash.digest('hex');
    if (actual !== expected) {
        throw new Error(
            `Binary integrity check failed for ${path.basename(binaryPath)}.\n` +
            `Expected: ${expected}\nActual:   ${actual}\n` +
            'The bundled binary may have been tampered with.'
        );
    }
}





export function validateBinaries(extensionPath: string): { valid: boolean; error?: string } {
    try {
        const binaryPath = resolveBinaryPath(extensionPath);
        ensureExecutable(binaryPath);
        verifyBinaryChecksum(binaryPath);
        return { valid: true };
    } catch (error) {
        return {
            valid: false,
            error: error instanceof Error ? error.message : String(error),
        };
    }
}
