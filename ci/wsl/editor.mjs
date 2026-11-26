// Drive the Windows workbench while its workspace extension host runs in WSL.
import assert from 'node:assert/strict';
import { setTimeout as wait } from 'node:timers/promises';

const [port, distro, workspace, cliTask, marker] = process.argv.slice(2);
const deadline = Date.now() + 300_000;
let sequence = 0;
let socket;
const requests = new Map();

async function until(label, action, timeout = 25_000) {
  const end = Math.min(deadline, Date.now() + timeout);
  let lastError;
  while (Date.now() < end) {
    try {
      const result = await action();
      if (result) return result;
    } catch (error) {
      lastError = error;
    }
    await wait(250);
  }
  throw new Error(`Timed out waiting for ${label}`, { cause: lastError });
}

async function connect() {
  const endpoint = await until('the VS Code workbench debugger', async () => {
    const response = await fetch(`http://127.0.0.1:${port}/json/list`, {
      signal: AbortSignal.timeout(3000),
    });
    const pages = await response.json();
    return pages.find(page => page.type === 'page' && /workbench[^/]*\.html/.test(page.url))?.webSocketDebuggerUrl;
  }, 60_000);
  assert.equal(new URL(endpoint).hostname, '127.0.0.1');
  socket = new WebSocket(endpoint);
  socket.addEventListener('message', event => {
    const message = JSON.parse(event.data);
    const request = requests.get(message.id);
    if (!request) return;
    requests.delete(message.id);
    if (message.error) request.reject(new Error(message.error.message));
    else request.resolve(message.result);
  });
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve, { once: true });
    socket.addEventListener('error', reject, { once: true });
  });
}

function send(method, params = {}) {
  const id = ++sequence;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      requests.delete(id);
      reject(new Error(`Debugger command timed out: ${method}`));
    }, 5000);
    requests.set(id, {
      resolve(value) { clearTimeout(timer); resolve(value); },
      reject(error) { clearTimeout(timer); reject(error); },
    });
    socket.send(JSON.stringify({ id, method, params }));
  });
}

async function evaluate(expression) {
  const reply = await send('Runtime.evaluate', { expression, returnByValue: true });
  if (reply.exceptionDetails) throw new Error(reply.exceptionDetails.text);
  return reply.result.value;
}

async function key(key, code, windowsVirtualKeyCode, modifiers = 0) {
  const event = { key, code, windowsVirtualKeyCode, modifiers };
  await send('Input.dispatchKeyEvent', { type: 'rawKeyDown', ...event });
  await send('Input.dispatchKeyEvent', { type: 'keyUp', ...event });
}

async function click(selector, label) {
  const point = await evaluate(`(() => {
    for (const element of document.querySelectorAll(${JSON.stringify(selector)})) {
      const area = element.getBoundingClientRect();
      const label = (element.innerText || element.getAttribute('aria-label') || '').trim();
      if (area.width && area.height && label.split('\\n').some(line => line.trim() === ${JSON.stringify(label)})) {
        return { x: area.left + area.width / 2, y: area.top + area.height / 2 };
      }
    }
    return null;
  })()`);
  if (!point) return false;
  await send('Input.dispatchMouseEvent', { type: 'mousePressed', ...point, button: 'left', clickCount: 1 });
  await send('Input.dispatchMouseEvent', { type: 'mouseReleased', ...point, button: 'left', clickCount: 1 });
  return true;
}

async function pick(label, timeout) {
  await until(label, async () => {
    const { target, focused } = await evaluate(`(() => {
      const input = document.activeElement;
      const rows = [...document.querySelectorAll('.quick-input-widget .monaco-list-row')]
        .filter(row => row.getBoundingClientRect().width && row.getBoundingClientRect().height);
      const target = rows.findIndex(row => row.innerText.split('\\n')
        .some(line => line.trim() === ${JSON.stringify(label)}));
      const focused = rows.findIndex(row => row.classList.contains('focused')
        || (row.id && row.id === input?.getAttribute('aria-activedescendant')));
      return { target, focused };
    })()`);
    if (target < 0) return false;
    if (focused === target) {
      await key('Enter', 'Enter', 13);
      return true;
    }
    const up = focused > target;
    await key(up ? 'ArrowUp' : 'ArrowDown', up ? 'ArrowUp' : 'ArrowDown', up ? 38 : 40);
    return false;
  }, timeout);
}

const quickInputFocused = "document.activeElement instanceof HTMLInputElement && !!document.activeElement.closest('.quick-input-widget')";
async function palette(label, timeout) {
  await key('P', 'KeyP', 80, 10); // Ctrl+Shift+P
  await until('Command Palette input', () => evaluate(quickInputFocused));
  await send('Input.insertText', { text: label });
  await pick(label, timeout);
}

const terminalBuffers = `[...document.querySelectorAll('.xterm')]
  .filter(terminal => terminal.getBoundingClientRect().width && terminal.getBoundingClientRect().height)
  .map(terminal => [...terminal.querySelectorAll('.xterm-accessibility-tree > *')]
    .map(row => row.textContent).join('\\n'))`;

function waitForTask(expected, previous) {
  return until('the completed scoped task in the VS Code terminal', async () => {
    for (const text of await evaluate(terminalBuffers)) {
      if (!text.includes(marker) || !text.includes('completed · access ready · exit 0')) continue;
      const id = text.match(/Visor task ([a-f0-9-]{36})/)?.[1];
      if (id && id !== previous && (!expected || id === expected)) return id;
    }
    return false;
  });
}

try {
  await connect();
  await until('the WSL workspace title', () => evaluate(`document.title.toLowerCase().includes(${JSON.stringify(distro.toLowerCase())})`), 120_000);
  await until('the Workspace Trust prompt', () =>
    click('.monaco-dialog-box .monaco-button, .monaco-dialog-box button', 'Yes, I trust the authors'), 60_000);
  const begin = async () => {
    await until('the installed Hypermesh command in the WSL window', async () => {
      try {
        await palette('Hypermesh: Visor terminal task', 6000);
        return true;
      } catch {
        // A palette opened during remote startup has a stale command list.
        await key('Escape', 'Escape', 27);
        return false;
      }
    }, 80_000);
    const title = `WSL ${distro} · ${workspace} · access ready`;
    await until('the distro and workspace in the task picker', () => evaluate(
      `document.querySelector('.quick-input-titlebar')?.innerText.includes(${JSON.stringify(title)})`,
    ));
  };
  await begin();
  await pick('Attach to task');
  await until('the task ID input', () => evaluate(
    `${quickInputFocused} && document.activeElement.closest('.quick-input-widget').innerText.includes('Task ID and optional output cursor')`,
  ));
  await send('Input.insertText', { text: cliTask });
  await key('Enter', 'Enter', 13);
  await waitForTask(cliTask);
  console.log(`VS Code attached to CLI task ${cliTask} in ${distro} at ${workspace}`);

  await begin();
  await pick('Run declared task');
  await until('the task review prompt', () =>
    click('.monaco-dialog-box .monaco-button, .monaco-dialog-box button', 'Run Task'));
  const editorTask = await waitForTask(undefined, cliTask);
  assert.notEqual(editorTask, cliTask);
  console.log(`VS Code ran task ${editorTask} in ${distro} at ${workspace}`);
  console.log(`VISOR_EDITOR_TASK=${editorTask}`);
} catch (error) {
  if (socket?.readyState === WebSocket.OPEN) {
    console.error(await evaluate(`JSON.stringify({
      title: document.title,
      terminalBuffers: (${terminalBuffers}).map(buffer => buffer.slice(-400)),
      visible: [...document.querySelectorAll('.quick-input-widget, .monaco-dialog-box, .notifications-toasts')]
        .filter(element => element.getBoundingClientRect().width)
        .map(element => element.innerText.slice(0, 600)),
    })`).catch(() => 'Workbench diagnostics unavailable'));
  }
  throw error;
} finally {
  socket?.close();
}
