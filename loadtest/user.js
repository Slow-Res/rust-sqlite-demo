// The load test (see docs/LOADTEST.md). Three models, chosen with MODE:
//   MODE=users  USERS=N  N virtual users. Each logs in, does 20-40 actions with
//                        1-5 s of think time after each, logs out, and starts
//                        a new session.
//   MODE=rps    RPS=R    R actions per second (ramping-arrival-rate), one action
//                        per iteration, no think time. No logins during the run:
//                        setup() logs in SESSION_POOL sessions one after the other
//                        and each VU uses one of them.
//   MODE=ramp  START, STEP, STEP_S, MAX_USERS  stairs of ramping-vus: START users,
//                        then STEP more every STEP_S seconds (a 10 s ramp up, then
//                        a hold). Each user logs in once, when it starts, and keeps
//                        the session for the whole run (no logout, no new login; a
//                        401 means the session was lost and it logs in again, counted
//                        as relogins). One action then think time, same mix as
//                        MODE=users. Every request is
//                        tagged step:<k> (k = elapsed / STEP_S, taken when the request
//                        starts) and scripts/loadtest.sh ramp reads the numbers per step.
// users and rps ramp for RAMP seconds, then hold for HOLD seconds. Every request is
// tagged phase:ramp|hold|post, action:<name> and kind:read|write|auth. All
// reported numbers come from phase:hold only.
import http from 'k6/http';
import exec from 'k6/execution';
import { sleep } from 'k6';
import { Counter, Rate } from 'k6/metrics';
import * as c from './common.js';

const MODE = __ENV.MODE || 'users';
const USERS = Number(__ENV.USERS || 20);
const RPS = Number(__ENV.RPS || 20);
const RAMP = Number(__ENV.RAMP || 60);
const HOLD = Number(__ENV.HOLD || 300);
const START = Number(__ENV.START || 100);
const STEP = Number(__ENV.STEP || 100);
const STEP_S = Number(__ENV.STEP_S || 60);
const MAX_USERS = Number(__ENV.MAX_USERS || 5000);
const STEP_RAMP_S = 10; // each step ramps up over this long, then holds
const P99_MS = Number(__ENV.P99_MS || 1000);
const SESSION_POOL = Number(__ENV.SESSION_POOL || 50);
const SEED_BOOKS = Number(__ENV.SEED_BOOKS || 50000);
const OUT = __ENV.OUT || '/out';
const PER_PAGE = 20;

// Every request kind with the statuses it may return. Anything else, a 5xx, a
// timeout or a network error is an error. 409 is a normal business answer.
const EXPECT = {
  login: [200], me: [200], logout: [204],
  browse_books: [200], search_books: [200], open_book: [200],
  list_members: [200], open_member: [200], list_loans: [200],
  borrow: [201, 409],            // 409: no copies left, or the member is at the loan limit
  return: [200, 409],            // only the user's own loans, so 409 (already returned) should not happen
  edit_book_get: [200], edit_book: [200],
  add_book: [201, 409],          // 409: duplicate ISBN (random ISBN, very rare)
  add_member: [201],
  edit_member_get: [200], edit_member: [200],
};
const KIND = {
  login: 'auth', me: 'auth', logout: 'auth',
  borrow: 'write', return: 'write', edit_book: 'write', add_book: 'write', add_member: 'write', edit_member: 'write',
};
const kindOf = (action) => KIND[action] || 'read';
const CALLBACK = {};
for (const a of Object.keys(EXPECT)) CALLBACK[a] = http.expectedStatuses(...EXPECT[a]);

// What the user intends to do, and how often (percent). The sub-requests it
// needs (a GET before a PUT, a page of active loans before a return) come on top.
const MIX = [
  ['browse_books', 20], ['search_books', 15], ['open_book', 10], ['members', 8], ['list_loans', 7],
  ['borrow', 13], ['return', 13], ['edit_book', 5], ['add_book', 4], ['add_member', 3], ['edit_member', 2],
];

const errors = new Rate('errors');
const actions = new Counter('actions'); // intended actions, tag intent
// Every answer that is not expected for its action: tags action, status (0 =
// network error or timeout) and phase.
const unexpected = new Counter('unexpected_status');
const converted = new Counter('return_converted_to_borrow'); // a return with no own loan to return
const relogins = new Counter('relogins'); // ramp mode: a login after a 401 (the session was lost), tag step
const skipped = new Counter('skipped_actions'); // e.g. a return with no active loan anywhere

// Thresholds that never fail. They exist so k6 keeps hold-only sub-metrics for
// the summary; scripts/loadtest.sh decides pass or fail from those numbers.
const thresholds = {};
const keep = (name, stat) => { thresholds[name] = [stat]; };
keep('http_req_duration{phase:hold}', 'p(95)>=0');
keep('http_reqs{phase:hold}', 'count>=0');
keep('errors{phase:hold}', 'rate>=0');
for (const kind of ['read', 'write', 'auth']) {
  keep(`http_req_duration{phase:hold,kind:${kind}}`, 'p(95)>=0');
  keep(`http_reqs{phase:hold,kind:${kind}}`, 'count>=0');
}
for (const [action, statuses] of Object.entries(EXPECT)) {
  keep(`http_req_duration{phase:hold,action:${action}}`, 'p(95)>=0');
  keep(`http_reqs{phase:hold,action:${action}}`, 'count>=0');
  keep(`errors{phase:hold,action:${action}}`, 'rate>=0');
  for (const s of statuses) keep(`http_reqs{phase:hold,action:${action},status:${s}}`, 'count>=0');
}
for (const [intent] of MIX) keep(`actions{phase:hold,intent:${intent}}`, 'count>=0');
keep('return_converted_to_borrow{phase:hold}', 'count>=0');
// k6 keeps a sub-metric only if a threshold names it, so list the statuses worth
// naming. Anything else still shows in errors{...}; level.jq reports it as "other".
const STATUSES = [0, 400, 401, 403, 404, 405, 408, 409, 413, 422, 429, 500, 502, 503, 504];
for (const action of Object.keys(EXPECT)) {
  for (const status of STATUSES) keep(`unexpected_status{phase:hold,action:${action},status:${status}}`, 'count>=0');
}

// Ramp mode: one set of sub-metrics per step, so k6 keeps them and its REST API
// serves them while the test runs (the runner stops the test at the first failing
// step). The p99 threshold never aborts: it is there so the metric's `tainted`
// flag says whether the step's p99 is over the limit, because the REST API gives
// no p99. The final numbers come from the end-of-test summary.
const STEPS = Math.floor((MAX_USERS - START) / STEP) + 1;
const stepThresholds = {};
if (MODE === 'ramp') {
  for (let k = 0; k < STEPS; k++) {
    stepThresholds[`http_req_duration{step:${k}}`] = [`p(99)<${P99_MS}`];
    stepThresholds[`http_req_failed{step:${k}}`] = ['rate>=0'];
    stepThresholds[`http_reqs{step:${k}}`] = ['count>=0'];
    stepThresholds[`relogins{step:${k}}`] = ['count>=0'];
  }
}
const stairs = [];
for (let k = 0; k < STEPS; k++) {
  stairs.push({ target: START + k * STEP, duration: `${STEP_RAMP_S}s` }, { target: START + k * STEP, duration: `${STEP_S - STEP_RAMP_S}s` });
}

const scenarios = MODE === 'ramp'
  ? {
    ramp: {
      executor: 'ramping-vus',
      exec: 'rampIteration',
      startVUs: 0,
      stages: stairs,
      gracefulRampDown: '0s',
      // The runner stops the test with SIGTERM 10 s after the failing step ended,
      // when every request of that step has finished (10 s timeout).
      gracefulStop: '2s',
    },
  }
  : MODE === 'rps'
  ? {
    rps: {
      executor: 'ramping-arrival-rate',
      exec: 'rpsIteration',
      startRate: 0,
      timeUnit: '1s',
      stages: [{ target: RPS, duration: `${RAMP}s` }, { target: RPS, duration: `${HOLD}s` }],
      // k6 hands out its VUs round-robin, so every pre-allocated VU logs in
      // during the ramp, all at about the same time; every login costs an Argon2
      // hash (CPU and about 19 MiB). Few pre-allocated VUs keep that burst
      // small (R/10 is enough for 100 ms per request). More VUs are started
      // when the node falls behind, up to the 2 s per request that fills 2R VUs;
      // dropped iterations only appear beyond that, when the node is saturated.
      preAllocatedVUs: Math.max(10, Math.ceil(RPS / 10)),
      maxVUs: RPS * 2 + 50,
      gracefulStop: '10s',
    },
  }
  : {
    users: {
      executor: 'ramping-vus',
      exec: 'userSession',
      startVUs: 0,
      stages: [{ target: USERS, duration: `${RAMP}s` }, { target: USERS, duration: `${HOLD}s` }],
      gracefulRampDown: '0s',
      gracefulStop: '10s',
    },
  };

export const options = {
  scenarios,
  thresholds: MODE === 'ramp' ? stepThresholds : thresholds,
  noCookiesReset: true, // the session cookie must survive between iterations in rps mode
  summaryTrendStats: ['avg', 'min', 'med', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

// Runs once: learn how big the database is, and tell the runner when the
// scenario starts. The runner reads that line to cut the samples it takes.
export function setup() {
  const res = c.login();
  if (res.status !== 200) throw new Error(`setup: login failed with ${res.status}`);
  const total = (path) => http.get(`${c.BASE_URL}${path}`).json('total');
  // rps: log in a fixed pool of sessions, one after the other (one Argon2 hash at
  // a time), before the scenario starts. Setup requests carry no phase tag.
  const sessions = [];
  if (MODE === 'rps') {
    const jar = http.cookieJar();
    for (let i = 0; i < SESSION_POOL; i++) {
      const r = c.login();
      const cookie = jar.cookiesForURL(c.BASE_URL)[c.SESSION_COOKIE];
      if (r.status !== 200 || !cookie) throw new Error(`setup: pool login ${i} failed with ${r.status}`);
      sessions.push(cookie[0]);
    }
  }
  const data = {
    sessions,
    books: total('/api/books?per_page=1'),
    members: total('/api/members?per_page=1'),
    loans: {
      active: total('/api/loans?status=active&per_page=1'),
      returned: total('/api/loans?status=returned&per_page=1'),
      all: total('/api/loans?status=all&per_page=1'),
    },
  };
  console.log(`LT_SETUP_DONE ${Date.now()} ${JSON.stringify(Object.assign({}, data, { sessions: sessions.length }))}`);
  return data;
}

// Ramp mode: the step the request starts in.
function stepOf() {
  return Math.floor((Date.now() - exec.scenario.startTime) / (STEP_S * 1000));
}

function phase() {
  if (MODE === 'ramp') return 'step'; // no ramp/hold/post phases; user sessions run until k6 stops
  const elapsed = (Date.now() - exec.scenario.startTime) / 1000;
  return elapsed < RAMP ? 'ramp' : elapsed < RAMP + HOLD ? 'hold' : 'post';
}

function request(method, path, body, action) {
  const ph = phase();
  const res = http.request(method, c.BASE_URL + path, body ? JSON.stringify(body) : null, {
    headers: { 'Content-Type': 'application/json' },
    timeout: '10s',
    responseCallback: CALLBACK[action],
    // `name` keeps k6 from making one series per URL (ids, search terms).
    tags: MODE === 'ramp'
      ? { name: action, action, kind: kindOf(action), phase: ph, step: String(stepOf()) }
      : { name: action, action, kind: kindOf(action), phase: ph },
  });
  const bad = !EXPECT[action].includes(res.status);
  errors.add(bad, { action, kind: kindOf(action), phase: ph });
  if (bad) unexpected.add(1, { action, status: String(res.status), phase: ph });
  // Ramp mode keeps one session per user: a 401 means it is gone, log in again.
  if (MODE === 'ramp' && res.status === 401 && action !== 'login') vu.loggedIn = false;
  return res;
}

const get = (path, action) => request('GET', path, null, action);
const json = (res, path) => {
  try { return path === undefined ? res.json() : res.json(path); } catch (_) { return undefined; }
};

// State one VU keeps across sessions.
const vu = { loggedIn: false, everLoggedIn: false, ready: false, loans: [] };

function startSession() {
  const res = request('POST', '/api/auth/login', { username: c.ADMIN_USER, password: c.ADMIN_PASSWORD }, 'login');
  vu.loggedIn = res.status === 200;
  if (vu.loggedIn) get('/api/auth/me', 'me');
  return vu.loggedIn;
}

function pageOf(total) {
  return 1 + c.rand(Math.max(1, Math.ceil(total / PER_PAGE)));
}

const ACTIONS = {
  browse_books(d) {
    get(`/api/books?page=${pageOf(d.books)}&per_page=${PER_PAGE}`, 'browse_books');
  },
  search_books() {
    // A word from the seeded titles or authors, or one whole seeded title.
    const q = Math.random() < 0.6 ? c.pick(c.SEARCH_WORDS) : c.titleOf(c.rand(SEED_BOOKS));
    get(`/api/books?q=${encodeURIComponent(q)}&per_page=${PER_PAGE}`, 'search_books');
  },
  open_book(d) {
    get(`/api/books/${1 + c.rand(d.books)}`, 'open_book');
  },
  members(d) {
    if (Math.random() < 0.5) get(`/api/members?page=${pageOf(d.members)}&per_page=${PER_PAGE}`, 'list_members');
    else get(`/api/members/${1 + c.rand(d.members)}`, 'open_member');
  },
  list_loans(d) {
    const status = c.pick(['active', 'returned', 'returned', 'all']);
    get(`/api/loans?status=${status}&page=${pageOf(d.loans[status])}&per_page=${PER_PAGE}`, 'list_loans');
  },
  borrow(d) {
    const res = request('POST', '/api/loans', { book_id: 1 + c.rand(d.books), member_id: 1 + c.rand(d.members) }, 'borrow');
    if (res.status === 201) vu.loans.push(json(res, 'id'));
  },
  return(d) {
    // Only a loan this VU borrowed itself, so users never fight over loans and
    // the number of active loans stays steady. With none left, borrow instead.
    const id = vu.loans.pop();
    if (id === undefined) {
      converted.add(1, { phase: phase() });
      this.borrow(d);
      return;
    }
    request('POST', `/api/loans/${id}/return`, null, 'return');
  },
  edit_book(d) {
    const id = 1 + c.rand(d.books);
    const book = json(get(`/api/books/${id}`, 'edit_book_get'));
    if (!book) return;
    // Same copies and ISBN, so the edit cannot clash with loans or other books.
    request('PUT', `/api/books/${id}`, {
      title: book.title, author: book.author, isbn: book.isbn,
      published_year: 1900 + c.rand(126), total_copies: book.total_copies,
    }, 'edit_book');
  },
  add_book() {
    request('POST', '/api/books', {
      title: `${c.pick(c.SEARCH_WORDS)} ${c.pick(c.SEARCH_WORDS)} ${c.rand(1e6)}`,
      author: `${c.pick(c.SEARCH_WORDS)} ${c.pick(c.SEARCH_WORDS)}`,
      isbn: c.randomIsbn(),
      published_year: 1900 + c.rand(126),
      total_copies: 1 + c.rand(5),
    }, 'add_book');
  },
  add_member() {
    const n = `${exec.vu.idInTest}-${Date.now()}-${c.rand(1e9)}`;
    request('POST', '/api/members', { name: `Load Member ${n}`, email: `lt-${n}@loadtest.example` }, 'add_member');
  },
  edit_member(d) {
    const id = 1 + c.rand(d.members);
    const member = json(get(`/api/members/${id}`, 'edit_member_get'));
    if (!member) return;
    request('PUT', `/api/members/${id}`, { name: `${member.name.slice(0, 150)} Jr`, email: member.email }, 'edit_member');
  },
};

// One action by the MIX percentages.
function act(d) {
  let roll = Math.random() * 100;
  let intent = MIX[MIX.length - 1][0];
  for (const [name, share] of MIX) {
    if (roll < share) { intent = name; break; }
    roll -= share;
  }
  actions.add(1, { intent, phase: phase() });
  ACTIONS[intent](d);
}

export function userSession(d) {
  if (!startSession()) {
    sleep(5);
    return;
  }
  const n = 20 + c.rand(21); // 20-40 actions
  for (let i = 0; i < n; i++) {
    if (phase() === 'post') return; // the hold is over; do not start more work
    act(d);
    sleep(1 + Math.random() * 4); // think time, never inside a timed request
  }
  request('POST', '/api/auth/logout', null, 'logout');
}

// Ramp mode: log in on the first iteration (and after a lost session), then one
// action and think time per iteration, for as long as k6 runs.
export function rampIteration(d) {
  if (!vu.loggedIn) {
    if (!startSession()) {
      sleep(5);
      return;
    }
    if (vu.everLoggedIn) relogins.add(1, { step: String(stepOf()) });
    vu.everLoggedIn = true;
  }
  act(d);
  sleep(1 + Math.random() * 4); // think time, never inside a timed request
}

export function rpsIteration(d) {
  if (!vu.ready) {
    // Use one of the sessions logged in by setup(); no login during the run.
    http.cookieJar().set(c.BASE_URL, c.SESSION_COOKIE, d.sessions[(__VU - 1) % d.sessions.length], { path: '/' });
    vu.ready = true;
  }
  act(d);
}

export function handleSummary(data) {
  return { [`${OUT}/k6-summary.json`]: JSON.stringify(data) };
}
