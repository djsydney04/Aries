#!/usr/bin/env node

const { execSync } = require('node:child_process');

const releaseType = process.argv[2] || 'patch';
const allowedTypes = new Set(['patch', 'minor', 'major']);

if (!allowedTypes.has(releaseType)) {
  console.error(`Invalid release type: ${releaseType}`);
  console.error('Use one of: patch, minor, major');
  process.exit(1);
}

execSync(`npm version ${releaseType} -m "chore(release): v%s"`, { stdio: 'inherit' });
