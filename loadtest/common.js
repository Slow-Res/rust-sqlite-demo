// Shared by seed.js and user.js: the API base URL, the staff login, and the
// generators for seeded data. Titles, authors, ISBNs and emails are pure
// functions of an index, so user.js can search for text that really exists
// without asking the database.
import http from 'k6/http';

export const BASE_URL = __ENV.BASE_URL || 'http://web:4000';
// LOCAL-ONLY test credentials. The account lives in a throwaway load-test
// database that scripts/loadtest.sh creates and deletes.
export const ADMIN_USER = __ENV.LT_ADMIN_USER || 'admin';
export const ADMIN_PASSWORD = __ENV.LT_ADMIN_PASSWORD || 'loadtest-local-only';

const ADJECTIVES = ['Crimson', 'Silent', 'Hidden', 'Golden', 'Broken', 'Distant', 'Electric', 'Frozen', 'Gentle', 'Hollow',
  'Iron', 'Jade', 'Kind', 'Lost', 'Midnight', 'Northern', 'Orange', 'Pale', 'Quiet', 'Rusty',
  'Scarlet', 'Tender', 'Unseen', 'Velvet', 'Wild', 'Yellow', 'Ancient', 'Bitter', 'Clever', 'Dusty',
  'Eager', 'Fading', 'Grand', 'Humble', 'Ivory', 'Jolly', 'Keen', 'Lonely', 'Mighty', 'Noble'];
const NOUNS = ['Harbor', 'Garden', 'Mirror', 'Engine', 'Forest', 'Letter', 'Island', 'Bridge', 'Lantern', 'Compass',
  'Orchard', 'Kingdom', 'River', 'Shadow', 'Season', 'Voyage', 'Whisper', 'Anchor', 'Candle', 'Desert',
  'Echo', 'Fortress', 'Glacier', 'Horizon', 'Journey', 'Labyrinth', 'Meadow', 'Nebula', 'Oracle', 'Palace',
  'Quarry', 'Refuge', 'Summit', 'Temple', 'Union', 'Valley', 'Wharf', 'Archive', 'Beacon', 'Citadel'];
const FIRST = ['Ada', 'Boris', 'Clara', 'Dmitri', 'Elena', 'Farid', 'Greta', 'Hugo', 'Ingrid', 'Jonas',
  'Keiko', 'Lars', 'Maya', 'Nils', 'Olga', 'Pavel', 'Quinn', 'Rosa', 'Sven', 'Tara',
  'Uma', 'Viktor', 'Wanda', 'Xavier', 'Yara', 'Zane', 'Amir', 'Bea', 'Carlos', 'Dana',
  'Emil', 'Fiona', 'Goran', 'Hana', 'Ivan', 'Julia', 'Karim', 'Lena', 'Marco'];
const SURNAMES = ['Abbott', 'Brandt', 'Castillo', 'Dvorak', 'Ellison', 'Fontaine', 'Gallagher', 'Hartmann', 'Ibsen', 'Jensen',
  'Kowalski', 'Lindqvist', 'Moreau', 'Nakamura', 'Okafor', 'Petrov', 'Quintero', 'Rasmussen', 'Silva', 'Tanaka',
  'Ulrich', 'Vargas', 'Whitaker', 'Xu', 'Yilmaz', 'Zielinski', 'Andersson', 'Bergstrom', 'Calderon', 'Delacroix',
  'Eriksen', 'Fischer', 'Gutierrez', 'Haddad', 'Ivanov', 'Jovanovic', 'Karlsson', 'Lombardi', 'Mancini'];

export const SEARCH_WORDS = [...ADJECTIVES, ...NOUNS, ...SURNAMES];

export function titleOf(i) {
  return `${ADJECTIVES[i % ADJECTIVES.length]} ${NOUNS[Math.floor(i / ADJECTIVES.length) % NOUNS.length]} ${i + 1}`;
}
export function authorOf(i) {
  return `${FIRST[i % FIRST.length]} ${SURNAMES[(i * 7 + 3) % SURNAMES.length]}`;
}
export function memberNameOf(i) {
  return `${FIRST[(i * 3) % FIRST.length]} ${SURNAMES[i % SURNAMES.length]} ${i + 1}`;
}
export function memberEmailOf(i) {
  return `member${i + 1}@loadtest.example`;
}

// ISBN-13: 12 digits plus a check digit (weights 1,3,1,3,...; sum divisible by 10).
export function isbn13(twelveDigits) {
  let sum = 0;
  for (let k = 0; k < 12; k++) sum += Number(twelveDigits[k]) * (k % 2 === 0 ? 1 : 3);
  return twelveDigits + String((10 - (sum % 10)) % 10);
}
// Seeded books use the 978 prefix with the book's index; books added during a
// load test use 979 with random digits, so the two never collide.
export function seedIsbn(i) {
  return isbn13('978' + String(i).padStart(9, '0'));
}
export function randomIsbn() {
  let digits = '979';
  for (let k = 0; k < 9; k++) digits += Math.floor(Math.random() * 10);
  return isbn13(digits);
}

export function rand(n) {
  return Math.floor(Math.random() * n);
}
export function pick(list) {
  return list[rand(list.length)];
}

const JSON_HEADERS = { 'Content-Type': 'application/json' };

// POST /api/auth/login. The session cookie lands in the VU's cookie jar.
export function login(params) {
  return http.post(
    `${BASE_URL}/api/auth/login`,
    JSON.stringify({ username: ADMIN_USER, password: ADMIN_PASSWORD }),
    Object.assign({ headers: JSON_HEADERS }, params),
  );
}
