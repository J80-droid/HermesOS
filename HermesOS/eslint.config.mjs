// HermesOS — ESLint flat config
//
// Architectuur Directief (Tauri v2 + React + Python sidecar):
// Lokale HTTP-calls naar loopback zijn verboden in de renderer. Gebruik
// @tauri-apps/api `invoke` voor communicatie met Rust/de sidecar. Externe
// HTTPS-URL's (OAuth, API's) blijven toegestaan.

import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import reactRefresh from 'eslint-plugin-react-refresh'
import tseslint from 'typescript-eslint'
import { defineConfig, globalIgnores } from 'eslint/config'

/** Matches common loopback URL prefixes in string literals (first arg to fetch/axios). */
const LOOPBACK_LITERAL_SELECTOR =
  "Literal[value=/^(https?:\\/\\/)?(127\\.0\\.0\\.1|localhost|\\[::1\\]|::1)(:\\d+)?(\\/|$)/i]"

const LOOPBACK_MSG =
  'Architectuur Directief (HermesOS/Tauri): geen HTTP naar loopback/localhost in de frontend. Gebruik IPC (`invoke` uit @tauri-apps/api) voor Rust/Python. Externe URL’s zijn wel toegestaan.'

export default defineConfig([
  globalIgnores([
    'dist',
    'src-tauri/target',
    'node_modules',
    'src/assets/vendor/**',
    'coverage',
  ]),
  {
    files: ['**/*.{ts,tsx,js,jsx}'],
    ignores: ['src/assets/vendor/**'],
    extends: [
      js.configs.recommended,
      ...tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite,
    ],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
    },
    rules: {
      'no-restricted-syntax': [
        'error',
        {
          // fetch('http://127.0.0.1/...') — first argument is a string literal
          selector: `CallExpression[callee.name='fetch'] > ${LOOPBACK_LITERAL_SELECTOR}`,
          message: LOOPBACK_MSG,
        },
        {
          // axios.get('http://localhost/...', ...) etc.
          selector: `CallExpression[callee.type='MemberExpression'][callee.object.name='axios'] > ${LOOPBACK_LITERAL_SELECTOR}`,
          message: LOOPBACK_MSG,
        },
      ],
    },
  },
])
