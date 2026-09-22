#!/usr/bin/env node
// Exercise the compiled CLI against the reviewed real Codex app-server, with
// an isolated home and a loopback Responses fixture. No account or model usage.
import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdir, mkdtemp, rm, stat, writeFile } from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const exec = promisify(execFile);
const root = fileURLToPath(new URL('..', import.meta.url));
const codex = process.env.CODEX_BIN ?? 'codex';
const cli = process.env.BIN ?? path.join(root, 'target/debug/codex-threads');
const version = (await exec(codex, ['--version'])).stdout.trim();
assert.equal(version, 'codex-cli 0.155.1', 'use the reviewed Codex release');
const temporary = await mkdtemp(path.join(os.tmpdir(), 'ct-offline-'));
const codexHome = path.join(temporary, 'codex');
const workspace = path.join(temporary, 'workspace');
const endpoint = `unix://${temporary}/app.sock`;
const config = path.join(temporary, 'client.toml');
const environment = {
  PATH: process.env.PATH,
  HOME: temporary,
  CODEX_HOME: codexHome,
  XDG_CONFIG_HOME: path.join(temporary, 'config'),
  XDG_STATE_HOME: path.join(temporary, 'state'),
  CODEX_THREADS_STATE: path.join(temporary, 'state'),
};
let requests = 0;
let daemon;
let daemonExit;
let daemonLog = '';
const fixture = http.createServer((request, response) => {
  if (request.method !== 'POST' || !request.url.endsWith('/responses')) {
    response.writeHead(404).end();
    return;
  }
  request.resume();
  request.on('end', () => {
    requests += 1;
    const id = `fixture-${requests}`;
    const events = [
      { type: 'response.created', response: { id } },
      { type: 'response.output_item.done', item: {
        type: 'message', role: 'assistant', id: `message-${requests}`,
        content: [{ type: 'output_text', text: 'codex-threads fixture complete' }],
      } },
      { type: 'response.completed', response: { id, usage: {
        input_tokens: 1, output_tokens: 1, total_tokens: 2,
      } } },
    ];
    response.writeHead(200, { 'content-type': 'text/event-stream', connection: 'close' });
    response.end(events.map(event => `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`).join(''));
  });
});

async function run(...args) {
  const { stdout } = await exec(cli, ['--config', config, ...args, '--json'], {
    env: environment, timeout: 45_000, maxBuffer: 4 * 1024 * 1024,
  });
  return JSON.parse(stdout);
}
async function startDaemon() {
  await rm(`${temporary}/app.sock`, { force: true });
  daemon = spawn(codex, ['app-server', '--listen', endpoint], {
    env: environment, stdio: ['ignore', 'pipe', 'pipe'],
  });
  daemonExit = once(daemon, 'exit');
  daemon.stdout.on('data', chunk => { daemonLog += chunk; });
  daemon.stderr.on('data', chunk => { daemonLog += chunk; });
  for (let attempt = 0; attempt < 100; attempt += 1) {
    if (daemon.exitCode !== null) throw new Error(`app-server exited: ${daemonLog}`);
    if (await stat(`${temporary}/app.sock`).then(s => s.isSocket(), () => false)) return;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  throw new Error(`app-server socket timeout: ${daemonLog}`);
}
async function stopDaemon() {
  if (!daemon || daemon.exitCode !== null) return;
  daemon.kill('SIGTERM');
  const timer = setTimeout(() => daemon.kill('SIGKILL'), 5000);
  try { await daemonExit; } finally { clearTimeout(timer); }
}
try {
  await Promise.all([mkdir(codexHome), mkdir(workspace)]);
  fixture.listen(0, '127.0.0.1');
  await once(fixture, 'listening');
  const port = fixture.address().port;
  await writeFile(path.join(codexHome, 'config.toml'), `
model = "fixture"
model_provider = "fixture"
approval_policy = "never"
sandbox_mode = "danger-full-access"
[model_providers.fixture]
name = "Offline fixture"
base_url = "http://127.0.0.1:${port}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
[features]
apps = false
plugins = false
`, { mode: 0o600 });
  await writeFile(config, `[servers.fixture]\nendpoint = ${JSON.stringify(endpoint)}\n`, { mode: 0o600 });
  await startDaemon();
  await run('servers', 'ping');
  const created = await run('new', '--cwd', workspace, '--name', 'Offline fixture', 'Say fixture complete');
  assert.equal(created.status, 'completed');
  assert.equal(created.finalAssistantText, 'codex-threads fixture complete');
  const id = created.threadId;
  assert.ok(id);
  const listed = await run('list');
  assert.ok(listed.threads.some(thread => thread.id === id));
  const detail = await run('show', id, '--items', 'full');
  assert.equal(detail.thread.id, id);
  assert.ok(detail.turns.data.length > 0);
  const occurrences = await run('search', 'messages', id, 'fixture');
  assert.ok(occurrences.occurrences.length > 0);
  assert.ok(occurrences.occurrences.every(item => item.turnId && item.itemId && item.turnCursor));
  const messages = await run('messages', id);
  assert.ok(JSON.stringify(messages).includes('codex-threads fixture complete'));
  await run('settings', 'set', id, '--effort', 'low');
  const settings = await run('settings', 'show', id);
  assert.equal(settings.effort, 'low');
  await run('name', id, 'Renamed fixture');
  assert.equal((await run('show', id)).thread.name, 'Renamed fixture');
  await run('goal', 'set', id, '--objective', 'Offline smoke goal', '--status', 'paused');
  assert.equal((await run('goal', 'get', id)).goal.objective, 'Offline smoke goal');
  assert.equal((await run('goal', 'clear', id)).cleared, true);
  const fork = await run('fork', id);
  assert.equal(fork.forkedFromThreadId, id);
  assert.notEqual(fork.threadId, id);
  const section = (await run('sections', 'create', 'Review')).section;
  assert.ok(section.id);
  assert.ok((await run('sections', 'list')).data.some(item => item.id === section.id));
  await run('sections', 'rename', section.id, 'Reviewed');
  await run('section', id, '--section', section.id);
  await run('section', fork.threadId, '--section', section.id, '--before', id);
  const sectionThreads = await run('list', '--section', section.id, '--sort', 'section-position', '--asc');
  assert.deepEqual(sectionThreads.threads.map(thread => thread.id), [fork.threadId, id]);
  assert.equal(sectionThreads.threads[0].section.name, 'Reviewed');
  await stopDaemon();
  await startDaemon();
  assert.equal((await run('show', id)).thread.section.id, section.id);
  await run('section', id, '--clear');
  assert.ok((await run('list', '--unsectioned')).threads.some(thread => thread.id === id));
  await run('sections', 'delete', section.id);
  assert.equal((await run('show', fork.threadId)).thread.section, null);
  const resumed = await run('send', id, 'Say fixture complete again');
  assert.equal(resumed.status, 'completed');
  assert.equal(resumed.finalAssistantText, 'codex-threads fixture complete');
  await run('archive', fork.threadId);
  await run('unarchive', fork.threadId);
  assert.equal(requests, 2, 'only the two requested turns reach the loopback fixture');
  console.log(JSON.stringify({ codex: version, modelRequests: requests, status: 'passed' }));
} catch (error) {
  console.error(daemonLog.slice(-8000));
  throw error;
} finally {
  await stopDaemon();
  fixture.closeAllConnections();
  await new Promise(resolve => fixture.close(resolve));
  await rm(temporary, { recursive: true, force: true });
}
