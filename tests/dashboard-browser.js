import { applicationUrls, filterServices, isRunning, needsAttention, safeHttpUrl, formatTimestamp } from '/app.js';

let passed = 0;
let failed = 0;
function check(name, run) {
  const item = document.createElement('li');
  try { run(); item.textContent = `PASS ${name}`; passed += 1; }
  catch (error) { item.textContent = `FAIL ${name}: ${error.message}`; failed += 1; }
  document.getElementById('checks').append(item);
}
function equal(actual, expected) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error(`${JSON.stringify(actual)} != ${JSON.stringify(expected)}`);
}
const services = [
  { id: 'sidecar', label: 'AkuSidecar', lifecycle: 'running', rootPid: 42, ports: [11122], health: { status: 'healthy' } },
  { id: 'offline', lifecycle: 'stopped', rootPid: null, ports: [], health: { status: 'unknown' } },
  { id: 'failure', lifecycle: 'failed', ports: [8080], health: { status: 'unhealthy' } },
];
check('valid deduplicated HTTP port links only', () => equal(applicationUrls([11122, 11122, 0, -1, 65536, 1.5, '8080', null, 65535]), [
  { url: 'http://127.0.0.1:11122/', port: 11122, inferred: true },
  { url: 'http://127.0.0.1:65535/', port: 65535, inferred: true },
]));
check('missing ports do not invent links', () => { equal(applicationUrls(undefined), []); equal(applicationUrls([]), []); });
check('running filter', () => equal(filterServices(services, '', 'running').map(s => s.id), ['sidecar']));
check('case-insensitive name, PID, and port search', () => {
  for (const query of ['AKUSIDECAR', '42', '11122']) equal(filterServices(services, query).map(s => s.id), ['sidecar']);
});
check('full inferred URL search', () => equal(filterServices(services, 'http://127.0.0.1:11122/').map(s => s.id), ['sidecar']));
check('attention keeps unknown distinct', () => { equal(needsAttention(services[1]), false); equal(needsAttention(services[2]), true); equal(isRunning(services[0]), true); });
check('URL protocols and credentials rejected', () => {
  for (const url of ['javascript:alert(1)', 'data:text/html,x', 'file:///C:/x', 'http://user:secret@localhost/']) equal(safeHttpUrl(url), null);
  equal(safeHttpUrl('http://127.0.0.1:11122/'), 'http://127.0.0.1:11122/');
});
check('unavailable timestamps stay unavailable', () => { for (const date of [undefined, null, '', 'invalid']) equal(formatTimestamp(date), 'Belum tersedia'); });
document.getElementById('result').textContent = `${passed} passed, ${failed} failed`;
document.documentElement.dataset.testResult = failed ? 'failed' : 'passed';
