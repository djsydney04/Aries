#!/usr/bin/env node

const { spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const binName = process.platform === 'win32' ? 'codex-mux.exe' : 'codex-mux';
const binaryPath = path.join(__dirname, '..', 'target', 'release', binName);

if (!fs.existsSync(binaryPath)) {
  console.error(`codex-mux binary not found at ${binaryPath}`);
  console.error('Run: npm run build');
  process.exit(1);
}

const child = spawn(binaryPath, process.argv.slice(2), { stdio: 'inherit' });

child.on('exit', (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});

child.on('error', (err) => {
  console.error(`Failed to launch codex-mux: ${err.message}`);
  process.exit(1);
});
