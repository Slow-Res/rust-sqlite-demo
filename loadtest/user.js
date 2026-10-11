// The load test (see docs/LOADTEST.md). Two models, chosen with MODE:
//   MODE=users  USERS=N  N virtual users. Each logs in, does 20-40 actions with
//                        1-5 s of think time after each, logs out, and starts
//                        a new session.
//   MODE=rps    RPS=R    R actions per second (ramping-arrival-rate), one action
//                        per iteration, no think time. Each VU logs in once.
// Both ramp for RAMP seconds, then hold for HOLD seconds. Every request is
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
  return: [200, 409],            // 409: the loan was already returned (by another user)
  return_lookup: [200],          // supporting: a page of active loans to pick one from
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

const scenarios = MODE === 'rps'
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
  thresholds,
  noCookiesReset: true, // the session cookie must survive between iterations in rps mode
  summaryTrendStats: ['avg', 'min', 'med', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

// Runs once: learn how big the database is, and tell the runner when the
// scenario starts. The runner reads that line to cut the samples it takes.
export function setup() {
  const res = c.login();
  if (res.status !== 200) throw new Error(`setup: login failed with ${res.status}`);
  const total = (path) => http.get(`${c.BASE_URL}${path}`).json('total');
  const data = {
    books: total('/api/books?per_page=1'),
    members: total('/api/members?per_page=1'),
    loans: {
      active: total('/api/loans?status=active&per_page=1'),
      returned: total('/api/loans?status=returned&per_page=1'),
      all: total('/api/loans?status=all&per_page=1'),
    },
  };
  console.log(`LT_SETUP_DONE ${Date.now()} ${JSON.stringify(data)}`);
  return data;
}

function phase() {
  const elapsed = (Date.now() - exec.scenario.startTime) / 1000;
  return elapsed < RAMP ? 'ramp' : elapsed < RAMP + HOLD ? 'hold' : 'post';
}

function request(method, path, body, action) {
  const ph = phase();
  const res = http.request(method, c.BASE_URL + path, body ? JSON.stringify(body) : null, {
    headers: { 'Content-Type': 'application/json' },
    timeout: '10s',
    responseCallback: CALLBACK[action],
    tags: { action, kind: kindOf(action), phase: ph },
  });
  errors.add(!EXPECT[action].includes(res.status), { action, kind: kindOf(action), phase: ph });
  return res;
}

const get = (path, action) => request('GET', path, null, action);
const json = (res, path) => {
  try { return path === undefined ? res.json() : res.json(path); } catch (_) { return undefined; }
};

// State one VU keeps across sessions.
const vu = { loggedIn: false, loans: [], activeTotal: null };

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
    if (res.status === 201) {
      vu.loans.push(json(res, 'id'));
      if (vu.loans.length > 50) vu.loans.shift();
    }
  },
  return(d) {
    let id = vu.loans.pop();
    if (id === undefined) {
      // No loan of our own: pick one from a random page of active loans.
      const total = vu.activeTotal === null ? d.loans.active : vu.activeTotal;
      const res = get(`/api/loans?status=active&page=${pageOf(total)}&per_page=${PER_PAGE}`, 'return_lookup');
      const items = json(res, 'items') || [];
      const newTotal = json(res, 'total');
      if (newTotal !== undefined) vu.activeTotal = newTotal;
      if (items.length === 0) {
        skipped.add(1, { intent: 'return', phase: phase() });
        return;
      }
      id = c.pick(items).id;
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

export function rpsIteration(d) {
  if (!vu.loggedIn && !startSession()) return;
  act(d);
}

export function handleSummary(data) {
  return { [`${OUT}/k6-summary.json`]: JSON.stringify(data) };
}
