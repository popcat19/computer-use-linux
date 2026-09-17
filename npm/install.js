#!/usr/bin/env node
'use strict';

const { execFileSync } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const https = require('node:https');
const os = require('node:os');
const path = require('node:path');

const pkg = require('../package.json');

const archToTarget = {
  x64: 'x86_64',
  arm64: 'aarch64',
};

const binDir = path.join(__dirname, 'bin');
const binaryPath = path.join(binDir, `computer-use-linux-${process.platform}-${process.arch}`);
const cosmicHelperPath = path.join(binDir, 'computer-use-linux-cosmic');

function fail(message) {
  console.error(`[computer-use-linux] ${message}`);
  process.exit(1);
}

function copyLocalBinary(source) {
  fs.mkdirSync(binDir, { recursive: true });
  fs.copyFileSync(source, binaryPath);
  fs.chmodSync(binaryPath, 0o755);
  if (process.env.COMPUTER_USE_LINUX_LOCAL_COSMIC_HELPER) {
    fs.copyFileSync(process.env.COMPUTER_USE_LINUX_LOCAL_COSMIC_HELPER, cosmicHelperPath);
    fs.chmodSync(cosmicHelperPath, 0o755);
  }
  console.log(`[computer-use-linux] installed local binary from ${source}`);
}

function buildFromSource(root) {
  console.log('[computer-use-linux] building checked-out Rust source (no release download)');
  const output = execFileSync('cargo', [
    'build', '--release', '--locked', '--bins', '--message-format=json',
  ], { cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 16 * 1024 * 1024 });
  const artifacts = new Map(output.trim().split('\n').filter(Boolean).map((line) => {
    const message = JSON.parse(line);
    return message.reason === 'compiler-artifact' && message.executable
      ? [message.target.name, message.executable] : [];
  }).filter((entry) => entry.length));
  const binary = artifacts.get('computer-use-linux');
  const helper = artifacts.get('computer-use-linux-cosmic');
  if (!binary || !helper) {
    throw new Error('cargo did not report both native executables');
  }
  fs.mkdirSync(binDir, { recursive: true });
  for (const [source, destination] of [[binary, binaryPath], [helper, cosmicHelperPath]]) {
    fs.copyFileSync(source, destination);
    fs.chmodSync(destination, 0o755);
  }
  console.log(`[computer-use-linux] installed source-built binary from ${binary}`);
}

function download(url, destination, redirects = 5) {
  return new Promise((resolve, reject) => {
    const request = https.get(url, (response) => {
      if (
        response.statusCode >= 300 &&
        response.statusCode < 400 &&
        response.headers.location &&
        redirects > 0
      ) {
        response.resume();
        const nextUrl = new URL(response.headers.location, url).toString();
        download(nextUrl, destination, redirects - 1).then(resolve, reject);
        return;
      }

      if (response.statusCode !== 200) {
        response.resume();
        reject(new Error(`download failed with HTTP ${response.statusCode}: ${url}`));
        return;
      }

      const file = fs.createWriteStream(destination, { mode: 0o600 });
      response.pipe(file);
      file.on('finish', () => file.close(resolve));
      file.on('error', reject);
    });
    request.on('error', reject);
  });
}

function parseSha256(text) {
  const match = text.match(/\b[a-fA-F0-9]{64}\b/);
  if (!match) {
    throw new Error('sha256 file did not contain a 64-character hex digest');
  }
  return match[0].toLowerCase();
}

function sha256File(filePath) {
  const hash = crypto.createHash('sha256');
  hash.update(fs.readFileSync(filePath));
  return hash.digest('hex');
}

async function main() {
  if (process.env.COMPUTER_USE_LINUX_SKIP_DOWNLOAD === '1') {
    console.log('[computer-use-linux] skipping binary download');
    return;
  }

  if (process.env.COMPUTER_USE_LINUX_LOCAL_BINARY) {
    copyLocalBinary(process.env.COMPUTER_USE_LINUX_LOCAL_BINARY);
    return;
  }

  if (process.platform !== 'linux') {
    fail(`unsupported platform: ${process.platform}. This package only supports Linux.`);
  }

  const targetArch = archToTarget[process.arch];
  if (!targetArch) {
    fail(`unsupported CPU architecture: ${process.arch}. Supported: x64, arm64.`);
  }

  // Git installs must run their checkout, not the same-version upstream release.
  const root = path.join(__dirname, '..');
  if (fs.existsSync(path.join(root, 'Cargo.toml'))) {
    buildFromSource(root);
    return;
  }

  const asset = `computer-use-linux-${targetArch}-unknown-linux-gnu`;
  const cosmicAsset = `computer-use-linux-cosmic-${targetArch}-unknown-linux-gnu`;
  const baseUrl =
    process.env.COMPUTER_USE_LINUX_DOWNLOAD_BASE ||
    `https://github.com/agent-sh/computer-use-linux/releases/download/v${pkg.version}`;
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'computer-use-linux-'));
  const tmpBinary = path.join(tmpDir, asset);
  const tmpSha = path.join(tmpDir, `${asset}.sha256`);
  const tmpCosmic = path.join(tmpDir, cosmicAsset);
  const tmpCosmicSha = path.join(tmpDir, `${cosmicAsset}.sha256`);

  try {
    console.log(`[computer-use-linux] downloading ${asset} from ${baseUrl}`);
    await download(`${baseUrl}/${asset}`, tmpBinary);
    await download(`${baseUrl}/${asset}.sha256`, tmpSha);
    await download(`${baseUrl}/${cosmicAsset}`, tmpCosmic);
    await download(`${baseUrl}/${cosmicAsset}.sha256`, tmpCosmicSha);

    const expected = parseSha256(fs.readFileSync(tmpSha, 'utf8'));
    const actual = sha256File(tmpBinary);
    if (actual !== expected) {
      fail(`sha256 mismatch for ${asset}: expected ${expected}, got ${actual}`);
    }

    const expectedCosmic = parseSha256(fs.readFileSync(tmpCosmicSha, 'utf8'));
    const actualCosmic = sha256File(tmpCosmic);
    if (actualCosmic !== expectedCosmic) {
      fail(`sha256 mismatch for ${cosmicAsset}: expected ${expectedCosmic}, got ${actualCosmic}`);
    }

    fs.mkdirSync(binDir, { recursive: true });
    fs.copyFileSync(tmpBinary, binaryPath);
    fs.chmodSync(binaryPath, 0o755);
    fs.copyFileSync(tmpCosmic, cosmicHelperPath);
    fs.chmodSync(cosmicHelperPath, 0o755);
    console.log(`[computer-use-linux] installed ${asset} and ${cosmicAsset}`);
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
}

main().catch((error) => fail(error.message));
