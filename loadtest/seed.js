// Fills the database through the HTTP API, so the change_log triggers,
// Litestream and the sync worker see real writes. One phase per run
// (PHASE=books|members|loans), started in that order by scripts/loadtest.sh.
//   books   BOOKS   books: 1-5 copies each, ISBN-13 for 4 in 5
//   members MEMBERS members
//   loans   LOANS   loans: loan j borrows book j+1 for member j mod MEMBERS. Loan j
//           stays active when j is a multiple of 10 and it is among the first two
//           of its member (j < 2 x MEMBERS); the rest are returned straight
//           away. So about 10% stay active, at most 2 per member (limit 5).
import http from 'k6/http';
import exec from 'k6/execution';
import { Counter } from 'k6/metrics';
import * as c from './common.js';

const PHASE = __ENV.PHASE;
const BOOKS = Number(__ENV.BOOKS || 50000);
const MEMBERS = Number(__ENV.MEMBERS || 10000);
const LOANS = Number(__ENV.LOANS || 20000);
const COUNT = { books: BOOKS, members: MEMBERS, loans: LOANS }[PHASE];
const seedErrors = new Counter('seed_errors');

export const options = {
  scenarios: {
    seed: {
      executor: 'shared-iterations',
      vus: Number(__ENV.SEED_VUS || 16),
      iterations: COUNT,
      maxDuration: '2h',
    },
  },
  // Any unexpected answer stops the run: a half-seeded database is useless.
  thresholds: { seed_errors: [{ threshold: 'count==0', abortOnFail: true }] },
  noCookiesReset: true,
};

let loggedIn = false;

function send(method, path, body, expected) {
  const res = http.request(method, c.BASE_URL + path, body ? JSON.stringify(body) : null, {
    headers: { 'Content-Type': 'application/json' },
    timeout: '60s',
    responseCallback: http.expectedStatuses(...expected),
  });
  if (!expected.includes(res.status)) {
    seedErrors.add(1);
    console.error(`seed: ${method} ${path} -> ${res.status} ${String(res.body).slice(0, 200)}`);
  }
  return res;
}

export function setup() {
  if (!COUNT) throw new Error('PHASE must be books, members or loans');
}

export default function () {
  if (!loggedIn) {
    const res = c.login();
    if (res.status !== 200) {
      seedErrors.add(1);
      console.error(`seed: login -> ${res.status}`);
      return;
    }
    loggedIn = true;
  }
  const i = exec.scenario.iterationInTest;
  if (PHASE === 'books') {
    send('POST', '/api/books', {
      title: c.titleOf(i),
      author: c.authorOf(i),
      isbn: i % 5 === 4 ? null : c.seedIsbn(i),
      published_year: 1900 + (i % 126),
      total_copies: 1 + (i % 5),
    }, [201]);
  } else if (PHASE === 'members') {
    send('POST', '/api/members', { name: c.memberNameOf(i), email: c.memberEmailOf(i) }, [201]);
  } else {
    const res = send('POST', '/api/loans', { book_id: (i % BOOKS) + 1, member_id: (i % MEMBERS) + 1 }, [201]);
    const stayActive = i % 10 === 0 && Math.floor(i / MEMBERS) < 2;
    if (res.status === 201 && !stayActive) {
      send('POST', `/api/loans/${res.json('id')}/return`, null, [200]);
    }
  }
}
