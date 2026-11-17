#!/usr/bin/env node
// 选轨决策的回归测试（npm run test:audio）。
//
// 做法：直接从 src/index.html 里把 `codecName` / `isDeadCodec` / `classifyAudio` /
// `audioPlan` / `needsAudioEntry` 这几个纯函数源码抠出来，在同一个 Function 作用域里
// eval 后跑断言。测的是真实代码而不是副本 —— 改了 index.html 测试立刻跟着变。
//
// 防的两次回归：
//   1) Chromium 只挑「第一条它能解的音频轨」，不是「默认轨」。双 AAC 文件把默认标在
//      第二条时，内核会静默播第一条；旧代码在「没有死编码轨」时直接 return，从不接管，
//      实测用户听到的是 440Hz 的第一条，而默认轨是 1000Hz 的第二条。
//   2) 音轨菜单入口用 hasDeadAudio() 判断，于是「没有死编码轨」= 菜单隐藏 =
//      用户连手动换轨的入口都没有，只能听着错的音轨。

import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const HTML = resolve(HERE, '..', 'src', 'index.html');
const src = readFileSync(HTML, 'utf8');

/** 从源码里抠出一个 `function name(...) {...}` 的完整文本（跳过字符串与注释）。 */
function extractFn(text, name) {
  const needle = 'function ' + name + '(';
  const at = text.indexOf(needle);
  if (at < 0) throw new Error(`index.html 里找不到 ${name}()：选轨决策逻辑没有被抽成可测的纯函数`);
  const open = text.indexOf('{', at);
  let depth = 0;
  let mode = null;
  for (let i = open; i < text.length; i++) {
    const ch = text[i];
    if (mode) {
      if (ch === '\\') { i++; continue; }
      if (mode === 'line') { if (ch === '\n') mode = null; continue; }
      if (mode === 'block') { if (ch === '*' && text[i + 1] === '/') { mode = null; i++; } continue; }
      if ((mode === 'sq' && ch === "'") || (mode === 'dq' && ch === '"') || (mode === 'bt' && ch === '`')) mode = null;
      continue;
    }
    if (ch === '/' && text[i + 1] === '/') { mode = 'line'; i++; continue; }
    if (ch === '/' && text[i + 1] === '*') { mode = 'block'; i++; continue; }
    if (ch === "'") { mode = 'sq'; continue; }
    if (ch === '"') { mode = 'dq'; continue; }
    if (ch === '`') { mode = 'bt'; continue; }
    if (ch === '{') depth++;
    else if (ch === '}') { depth--; if (depth === 0) return text.slice(at, i + 1); }
  }
  throw new Error(`函数 ${name} 的大括号不配平`);
}

const NAMES = ['codecName', 'isDeadCodec', 'classifyAudio', 'audioPlan', 'needsAudioEntry'];
const body = NAMES.map((n) => extractFn(src, n)).join('\n');
const { classifyAudio, audioPlan, needsAudioEntry } = new Function(`${body}\nreturn { ${NAMES.join(', ')} };`)();

const A = (codec, def) => ({ kind: 'audio', codec, default: !!def });
const S = (codec) => ({ kind: 'subtitle', codec });

let failed = 0;
function check(label, got, want) {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${label.padEnd(46)} got=${JSON.stringify(got)} want=${JSON.stringify(want)}`);
  if (!ok) failed++;
}

// ---- classifyAudio.pin：内核会不会选中非默认轨 ----
const pinCases = [
  ['单 AAC 默认轨', [A('A_AAC', true)], false],
  ['单 AAC 无 default 标记', [A('A_AAC')], false],
  ['双 AAC 默认在第二条（内核会播第一条）', [A('A_AAC'), A('A_AAC', true)], true],
  ['双 AAC 默认在第一条（内核行为正确）', [A('A_AAC', true), A('A_AAC')], false],
  ['AC3 + 默认 DTS（内核跳过 DTS 播 AC3）', [A('A_AC3'), A('A_DTS', true)], true],
  ['默认 DTS + AC3（内核跳过 DTS 播 AC3）', [A('A_DTS', true), A('A_AC3')], true],
  // 单条 DTS：根本没有可解的轨，仍然要接管（转码），所以 pin 为真
  ['单 DTS（无可解轨，仍需接管）', [A('A_DTS', true)], true],
  ['无音轨', [], false],
  ['只有字幕轨', [S('S_TEXT/UTF8')], false],
];
for (const [label, tracks, want] of pinCases) {
  check(`pin: ${label}`, classifyAudio(tracks).pin, want);
}

// ---- needsAudioEntry：音轨菜单入口是否出现 ----
const entryCases = [
  ['单 AAC 默认轨（无入口）', [A('A_AAC', true)], false],
  ['双 AAC 默认在第二条（必须给入口）', [A('A_AAC'), A('A_AAC', true)], true],
  ['单 DTS（有默认轨解不了，给入口）', [A('A_DTS', true)], true],
  // 5 条全 AC-3、默认第 1 条：内核能解、也选对了（dead=0 且 pin=false），但用户就是
  // 想换一条语言听。以前判据只有 dead/pin，这里返回 false —— 7.5 GB 的 5 音轨片子
  // 连音轨按钮都不出现（2026-11-17 实测），用户根本无从切轨。
  ['5 条全 AC-3（都能解、默认也对，仍要给入口）',
    [A('A_AC3', true), A('A_AC3'), A('A_AC3'), A('A_AC3'), A('A_AC3')], true],
  ['无音轨', [], false],
];
for (const [label, tracks, want] of entryCases) {
  check(`entry: ${label}`, needsAudioEntry(tracks), want);
}

// 「要不要显示入口」和「要不要接管音频管线」是两件独立的事：
// 5 条 AC-3 不需要接管（内核自己就会选对轨，native 仍为 true），但仍然要给入口。
check('plan: 5 条全 AC-3 仍交给内核',
  audioPlan([A('A_AC3', true), A('A_AC3'), A('A_AC3'), A('A_AC3'), A('A_AC3')]).native, true);

// ---- audioPlan：要不要接管（隔离原轨 + 转外挂 mka） ----
check('plan: 单 AAC 交给内核', audioPlan([A('A_AAC', true)]), {
  count: 1, pin: false, index: 0, defDead: false, label: 'A_AAC', shake: [], native: true,
});
check('plan: 双 AAC 默认在第二条，必须接管', audioPlan([A('A_AAC'), A('A_AAC', true)]), {
  count: 2, pin: true, index: 1, defDead: false, label: 'A_AAC', shake: [], native: false,
});
check('plan: 默认 DTS（解不了），接管第 2 条', audioPlan([A('A_AC3'), A('A_DTS', true)]), {
  count: 2, pin: true, index: 1, defDead: true, label: 'DTS（含 DTS-HD）', shake: ['AC-3'], native: false,
});
check('plan: 无音轨', audioPlan([]), {
  count: 0, pin: false, index: 0, defDead: false, label: '', shake: [], native: true,
});

if (failed) {
  console.error(`\n${failed} 项失败`);
  process.exit(1);
}
console.log('\n全部通过');
