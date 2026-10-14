#!/usr/bin/env node
// Exercise the compiled CLI against the reviewed real Codex app-server, with
// an isolated home and a loopback Responses fixture. No account or model usage.
import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { once } from 'node:events';
import { lstat, mkdir, mkdtemp, rm, stat, writeFile } from 'node:fs/promises';
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
assert.equal(version, 'codex-cli 0.162.1', 'use the reviewed Codex release');
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
const modelInputs = [];
const partialAnswer = 'codex-threads fixture partial';
const finalAnswer = 'codex-threads fixture complete';
const combinedAnswer = `${partialAnswer}\n${finalAnswer}`;
let daemon;
let daemonExit;
let daemonLog = '';
const fixture = http.createServer((request, response) => {
  if (request.method !== 'POST' || !request.url.endsWith('/responses')) {
    response.writeHead(404).end();
    return;
  }
  const chunks = [];
  request.on('data', chunk => { chunks.push(chunk); });
  request.on('end', () => {
    requests += 1;
    modelInputs.push(JSON.parse(Buffer.concat(chunks).toString('utf8')));
    const id = `fixture-${requests}`;
    const events = [
      { type: 'response.created', response: { id } },
      { type: 'response.output_item.done', item: {
        type: 'message', role: 'assistant', id: `partial-${requests}`,
        phase: 'partial_answer',
        content: [{ type: 'output_text', text: partialAnswer }],
      } },
      { type: 'response.output_item.done', item: {
        type: 'message', role: 'assistant', id: `final-${requests}`,
        phase: 'final_answer',
        content: [{ type: 'output_text', text: finalAnswer }],
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
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    if (daemon.exitCode !== null) throw new Error(`app-server exited: ${daemonLog}`);
    if (await stat(`${temporary}/app.sock`).then(s => s.isSocket(), () => false)) {
      assert.ok((await lstat(`${temporary}/app.sock`)).isSymbolicLink(),
        'the reviewed Codex release must expose a symlink socket');
      return;
    }
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
enable_request_compression = false
`, { mode: 0o600 });
  await writeFile(config, `[servers.fixture]\nendpoint = ${JSON.stringify(endpoint)}\n`, { mode: 0o600 });
  await startDaemon();
  await run('servers', 'ping');
  const models = await run('models');
  assert.ok(Array.isArray(models.models));
  assert.ok(models.models.length > 0, 'real app-server returns its model catalog');
  assert.ok(models.models.every(model => typeof model.id === 'string' && typeof model.model === 'string'));
  const created = await run('new', '--cwd', workspace, '--name', 'Offline fixture', 'Say fixture complete');
  assert.equal(created.status, 'completed');
  assert.equal(created.finalAssistantText, combinedAnswer);
  const id = created.threadId;
  assert.ok(id);
  const listed = await run('list');
  assert.ok(listed.threads.some(thread => thread.id === id));
  const detail = await run('show', id, '--items', 'full');
  assert.equal(detail.thread.id, id);
  assert.ok(detail.turns.data.length > 0);
  const firstTurn = detail.turns.data.find(turn => turn.id === created.turnId);
  assert.equal(firstTurn.rootTurnId, created.turnId, 'direct user work starts its own turn lineage');
  assert.ok(firstTurn.items.some(item => item.type === 'agentMessage'
    && item.phase === 'partial_answer' && item.text === partialAnswer));
  assert.ok(firstTurn.items.some(item => item.type === 'agentMessage'
    && item.phase === 'final_answer' && item.text === finalAnswer));
  const occurrences = await run('search', 'messages', id, 'fixture');
  assert.ok(occurrences.occurrences.length > 0);
  assert.ok(occurrences.occurrences.every(item => item.turnId && item.itemId && item.turnCursor));
  const partialOccurrences = await run('search', 'messages', id, partialAnswer);
  assert.ok(partialOccurrences.occurrences.some(item => item.turnId === created.turnId),
    'occurrence search includes partial assistant answers');
  const finalOccurrences = await run('search', 'messages', id, finalAnswer);
  assert.ok(finalOccurrences.occurrences.some(item => item.turnId === created.turnId),
    'occurrence search includes final assistant answers');
  const messages = await run('messages', id);
  assert.ok(messages.messages.some(message => message.role === 'assistant' && message.text === partialAnswer));
  assert.ok(messages.messages.some(message => message.role === 'assistant' && message.text === finalAnswer));
  await run('settings', 'set', id, '--effort', 'low');
  const settings = await run('settings', 'show', id);
  assert.equal(settings.effort, 'low');
  await run('name', id, 'Renamed fixture');
  assert.equal((await run('show', id)).thread.name, 'Renamed fixture');
  const automaticObjective = 'Automatically generated offline smoke goal';
  await run('goal', 'set', id, '--objective', automaticObjective, '--status', 'paused');
  assert.equal((await run('goal', 'get', id)).goal.objective, automaticObjective);
  assert.equal((await run('goal', 'clear', id, '--origin', 'automatic')).cleared, true);
  const userObjective = 'Draft the offline smoke report';
  const updatedUserObjective = 'Revise the offline smoke report';
  await run('goal', 'set', id, '--objective', userObjective, '--status', 'paused', '--origin', 'user');
  await run('goal', 'set', id, '--objective', updatedUserObjective, '--status', 'paused', '--origin', 'user');
  assert.equal((await run('goal', 'get', id)).goal.objective, updatedUserObjective);
  assert.equal((await run('goal', 'clear', id, '--origin', 'user')).cleared, true);
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
  assert.equal(resumed.finalAssistantText, combinedAnswer);
  const resumedDetail = await run('show', id, '--items', 'full');
  assert.equal(resumedDetail.turns.data.find(turn => turn.id === resumed.turnId).rootTurnId, resumed.turnId);
  await run('archive', fork.threadId);
  await run('unarchive', fork.threadId);
  assert.equal(requests, 2, 'only the two requested turns reach the loopback fixture');
  const userHistory = modelInputs[1].input
    .filter(item => item.type === 'message' && item.role === 'user')
    .flatMap(item => item.content.map(content => content.text ?? ''))
    .join('\n');
  assert.ok(!userHistory.includes(automaticObjective), 'automatic goal edits supply no user authorization');
  const originalGoalPosition = userHistory.indexOf(`User set the goal: ${JSON.stringify(userObjective)}`);
  const updatedGoalPosition = userHistory.indexOf(`User set the goal: ${JSON.stringify(updatedUserObjective)}`);
  assert.ok(originalGoalPosition >= 0 && updatedGoalPosition > originalGoalPosition,
    'explicit user goal edits persist in order across app-server restart');
  assert.ok(userHistory.includes('User set goal status: "paused".'));
  assert.equal(userHistory.split('User cleared the goal.').length - 1, 1,
    'only the explicit user clear appends a user instruction');
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
