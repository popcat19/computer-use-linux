// Purpose: Verify Git installs build their checkout without a release fallback.
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { test } = require('node:test');
const vm = require('node:vm');

async function runInstaller({ buildError, missingHelper = false, env = {} } = {}) {
  const calls = { builds: [], copies: [], modes: [], failures: [], downloads: [] };
  const root = '/checkout';
  const context = {
    __dirname: `${root}/npm`,
    console: { log() {}, error(message) { calls.failures.push(message); } },
    process: { platform: 'linux', arch: 'x64', env, exit(code) { calls.exit = code; } },
    require(name) {
      if (name === '../package.json') return { version: '0.7.0' };
      if (name === 'node:fs') return {
        existsSync: (file) => file === `${root}/Cargo.toml`,
        mkdirSync() {},
        copyFileSync: (...args) => calls.copies.push(args),
        chmodSync: (...args) => calls.modes.push(args),
      };
      if (name === 'node:https') return { get() { calls.downloads.push(true); throw new Error('unexpected download'); } };
      if (name === 'node:child_process') return {
        execFileSync(...args) {
          calls.builds.push(args);
          if (buildError) throw new Error(buildError);
          return ['computer-use-linux', ...(missingHelper ? [] : ['computer-use-linux-cosmic'])]
            .map((name) => JSON.stringify({ reason: 'compiler-artifact', target: { name }, executable: `/custom-target/release/${name}` }))
            .join('\n');
        },
      };
      return require(name);
    },
  };
  vm.runInNewContext(fs.readFileSync(path.join(__dirname, 'install.js'), 'utf8'), context);
  await new Promise((resolve) => setImmediate(resolve));
  return calls;
}

test('Git checkout builds locked release binaries and installs actual Cargo artifact paths', async () => {
  const calls = await runInstaller();
  assert.equal(calls.builds.length, 1);
  const [command, args, options] = calls.builds[0];
  assert.equal(command, 'cargo');
  assert.deepEqual(Array.from(args), ['build', '--release', '--locked', '--bins', '--message-format=json']);
  assert.equal(options.cwd, '/checkout');
  assert.deepEqual(calls.copies, [
    ['/custom-target/release/computer-use-linux', '/checkout/npm/bin/computer-use-linux-linux-x64'],
    ['/custom-target/release/computer-use-linux-cosmic', '/checkout/npm/bin/computer-use-linux-cosmic'],
  ]);
  assert.ok(calls.modes.every(([, mode]) => mode === 0o755));
  assert.deepEqual(calls.failures, []);
  assert.deepEqual(calls.downloads, []);
});

test('source build failure fails installation without downloading upstream binaries', async () => {
  const calls = await runInstaller({ buildError: 'cargo failed' });
  assert.equal(calls.exit, 1);
  assert.match(calls.failures.join(), /cargo failed/);
  assert.deepEqual(calls.copies, []);
  assert.deepEqual(calls.downloads, []);
});

test('missing source artifact fails installation without partial copying or download', async () => {
  const calls = await runInstaller({ missingHelper: true });
  assert.equal(calls.exit, 1);
  assert.match(calls.failures.join(), /both native executables/);
  assert.deepEqual(calls.copies, []);
  assert.deepEqual(calls.downloads, []);
});

test('explicit local binary and skip overrides retain precedence over source build', async () => {
  for (const env of [{ COMPUTER_USE_LINUX_SKIP_DOWNLOAD: '1' }, { COMPUTER_USE_LINUX_LOCAL_BINARY: '/explicit/binary' }]) {
    const calls = await runInstaller({ env });
    assert.deepEqual(calls.builds, []);
    assert.deepEqual(calls.downloads, []);
    assert.deepEqual(calls.failures, []);
    assert.equal(calls.copies.length, env.COMPUTER_USE_LINUX_LOCAL_BINARY ? 1 : 0);
  }
});
