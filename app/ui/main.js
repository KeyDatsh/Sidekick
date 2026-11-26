'use strict';
// The page only draws state and calls commands; all rules live in the Rust core.
// User text always goes through textContent/value, never innerHTML.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const ui = { active: false, view: 'tasks', day: null, reportDay: null, editing: null, draft: null, snap: null };
// Subtitles arrive as events between refreshes; the snapshot brings back the saved ones after a reload.
const meeting = { status: { running: false, note: '', error: false }, lines: [], partial: null };
// Pause mode: `held` while the key is down, `partial` meanwhile, `last` the finished phrase.
const pause = { held: false, meeting: false, partial: null, last: null };
// The finished pause phrase stays on screen this long, so it can be read out to the call.
const PAUSE_SHOWN_MS = 30_000;
const root = document.getElementById('app');

// Small DOM helper: h('button.primary', { onclick }, 'text', child…)
function h(spec, props, ...children) {
  const [tag, ...classes] = spec.split('.');
  const el = document.createElement(tag || 'div');
  el.className = classes.join(' ');
  for (const [k, v] of Object.entries(props || {})) {
    if (k in el) el[k] = v;
    else el.setAttribute(k, v);
  }
  el.append(...children.flat().filter((c) => c !== null && c !== undefined && c !== false));
  return el;
}

let toastTimer;
function toast(text, info) {
  const el = document.getElementById('toast');
  el.textContent = text;
  el.className = info ? 'on info' : 'on';
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.className = ''), 5000);
}

async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    toast(String(e));
    throw e;
  }
}

// Runs an action and redraws; errors are already on screen.
const act = (fn) => async (...args) => {
  try {
    await fn(...args);
    await refresh();
  } catch (e) {
    console.error(e);
  }
};

// Dates

const WEEKDAYS = ['Нд', 'Пн', 'Вт', 'Ср', 'Чт', 'Пт', 'Сб'];
const date = (iso) => new Date(iso + 'T00:00:00');
const isoOf = (d) =>
  `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
const addDays = (iso, n) => {
  const d = date(iso);
  d.setDate(d.getDate() + n);
  return isoOf(d);
};
const dayShort = (iso) => (iso === null ? 'Пізніше' : `${WEEKDAYS[date(iso).getDay()]} ${date(iso).getDate()}`);
const dayName = (iso) => {
  if (iso === ui.snap.today) return 'Сьогодні';
  if (iso === addDays(ui.snap.today, 1)) return 'Завтра';
  return dayShort(iso);
};
const dayLong = (iso) => date(iso).toLocaleDateString('uk-UA', { weekday: 'long', day: 'numeric', month: 'long' });

const projectLabel = (id) => {
  const p = ui.snap.projects.find((p) => p.id === id);
  return p ? (p.name ? `${p.client} · ${p.name}` : p.client) : '';
};
const carried = (t) => {
  if (!t.carried_from) return '';
  return t.carried_from === addDays(ui.snap.today, -1) ? 'з учора' : `з ${dayShort(t.carried_from)}`;
};
const hotkeyLabel = (s) => s.replace(/Key|Digit/g, '').replaceAll('+', ' + ');

// Data and drawing

let refreshing = 0;
async function refresh() {
  const mine = ++refreshing;
  const snap = await call('snapshot', { day: ui.day === 'later' ? null : ui.day ?? (ui.snap && ui.snap.today) });
  if (mine !== refreshing) return; // a newer refresh is on its way
  const firstLoad = !ui.snap;
  const newDay = ui.snap && ui.snap.today !== snap.today;
  ui.snap = snap;
  meeting.status = snap.meeting.status;
  if (snap.meeting.lines.length >= meeting.lines.length || !meeting.status.running) {
    meeting.lines = snap.meeting.lines.map((l) => (l.speaker === 'me'
      ? { me: true, uk: l.original, en: l.translation }
      : { uk: l.translation, en: l.original }));
  }
  if (firstLoad || newDay) {
    ui.day = snap.today;
    if (firstLoad && snap.active) {
      ui.active = true;
      if (!snap.hotkeys_ok) ui.view = 'settings';
    }
    return refresh();
  }
  draw();
}

function draw() {
  document.body.classList.toggle('active', ui.active);
  document.documentElement.style.setProperty('--opacity', ui.snap.settings.opacity / 100);
  const view = ui.active ? views[ui.view] : inactive;
  root.replaceChildren(...view().filter(Boolean));
  for (const el of root.querySelectorAll('.subs')) el.scrollTop = el.scrollHeight;
  const focus = root.querySelector('[data-focus]');
  if (ui.active && focus) focus.focus();
}

function go(view) {
  ui.view = view;
  ui.editing = null;
  refresh().catch(console.error);
}

function drag(el) {
  el.setAttribute('data-tauri-drag-region', '');
  return el;
}

function header() {
  const tab = (view, text) => h(`button.ghost${ui.view === view ? '.on' : ''}`, { onclick: () => go(view) }, text);
  return drag(
    h('div.row.tabs', {}, tab('tasks', 'Задачі'), tab('report', 'Звіт'), tab('plan', 'План дня'),
      tab('meeting', meeting.status.running ? '● Мітинг' : 'Мітинг'), drag(h('span.grow')),
      h('button.ghost', { onclick: () => go('settings'), ariaLabel: 'Налаштування', title: 'Налаштування' }, '⚙'),
      h('button.ghost', { onclick: () => invoke('deactivate'), title: 'Сховати (Esc)' }, 'Esc')),
  );
}

// Subtitle lines, newest at the bottom; the unfinished phrase in grey.
function subtitles(limit) {
  // Your own lines (pause mode): the English is what matters, the Ukrainian is the reminder.
  // The newest one of them is already in the pause card while that is shown.
  const all = pauseCard() && meeting.lines.at(-1)?.me ? meeting.lines.slice(0, -1) : meeting.lines;
  const lines = all.slice(-limit).map((l) => (l.me
    ? h('div.sub.me', {}, h('div.uk', {}, l.en), h('div.en', {}, `Ти: ${l.uk}`))
    : h('div.sub', {}, h('div.uk', {}, l.uk), h('div.en', {}, l.en))));
  const p = meeting.partial;
  return h('div.subs', {}, lines, p && h('div.sub.partial', {}, h('div.uk', {}, p.uk), h('div.en', {}, p.en)),
    !lines.length && !p && h('span.muted', {}, 'Чекаю, поки хтось заговорить…'));
}

function pauseCard() {
  if (pause.held) {
    const p = pause.partial;
    return h('div.card.pausing', {},
      h('b', {}, 'Пауза: тебе чує тільки асистент'),
      !pause.meeting && h('span.muted', {}, 'Мітинг не запущено, тож перекладу не буде. Дзвінок тебе все одно не чує.'),
      p && h('div.say', {}, p.en), p && h('span.muted', {}, p.uk));
  }
  const last = pause.last;
  if (!last || Date.now() - last.at > PAUSE_SHOWN_MS) return null;
  return h('div.card.said', {},
    last.en ? h('div.say', {}, last.en) : h('span.muted', {}, 'Не розчув, спробуй ще раз.'),
    last.uk && h('span.muted', {}, last.uk));
}

function meetingNote() {
  const s = meeting.status;
  return s.note && h(`span.grow.clip${s.error ? '.bad' : '.muted'}`, { title: s.note }, s.note);
}

function inactive() {
  if (meeting.status.running || pauseCard()) {
    return [
      meeting.status.running ? subtitles(pauseCard() ? 3 : 6) : h('span.grow'),
      pauseCard(),
      h('div.row', {}, meetingNote() || h('span.grow'), h('span.dim', {}, `${hotkeyLabel(ui.snap.settings.toggle_hotkey)}: керування`)),
    ];
  }
  const tasks = ui.snap.today_tasks;
  const open = tasks.filter((t) => !t.done);
  return [
    h('div.row', {}, h('b.grow', {}, `Сьогодні · ${tasks.length - open.length} з ${tasks.length} готово`),
      h('span.muted', {}, dayShort(ui.snap.today))),
    h('div.scroll', {}, open.slice(0, 8).map((t) =>
      h('div.row', {}, h('span.dot'), h('span.grow.clip', {}, t.title), h('span.muted', {}, projectLabel(t.project_id))))),
    h('div.muted', {}, `${hotkeyLabel(ui.snap.settings.toggle_hotkey)}, щоб активувати. Мишка проходить крізь вікно.`),
  ];
}

function taskRow(t) {
  const start = act(() => call('set_started', { id: t.id, started: !t.started }));
  const days = ui.snap.tabs.map((tab) => tab.day).filter((d) => d !== t.day);
  const move = h('select', {
    title: 'Перенести',
    style: 'width:auto;padding:1px 4px;font-size:11px',
    onchange: act((e) => call('move_task', { id: t.id, day: e.target.value || null })),
  }, h('option', { disabled: true, selected: true }, 'на…'), days.map((d) => h('option', { value: d ?? '' }, d ? dayName(d) : 'Пізніше')));
  return h(`div.task${t.done ? '.done' : ''}`, {},
    h(`button.check${t.done ? '.done' : t.started ? '.started' : ''}`, {
      title: t.done ? 'Відкрити знову' : 'Готово',
      onclick: act(() => call('set_done', { id: t.id, done: !t.done })),
    }, t.done ? '✓' : ''),
    h('button.title.clip', { title: t.body || t.title, onclick: () => { ui.editing = t; ui.view = 'edit'; draw(); } }, t.title),
    t.started && !t.done && h('span.dim', {}, 'в роботі'),
    h('span.dim', {}, carried(t)),
    !t.done && h('span.acts', {},
      h('button', { onclick: start, title: 'Позначити, що робота почалась (потрапить у звіт)' }, t.started ? '■' : '▶'), move),
    projectLabel(t.project_id) && h('span.chip', {}, projectLabel(t.project_id)));
}

// Project picker shared by the quick task and the editor. `form.project` is the result.
function projectPicker(form) {
  const box = h('div.col');
  let filter = '';
  let adding = false;
  const paint = () => {
    const shown = ui.snap.projects.filter((p) => projectLabel(p.id).toLowerCase().includes(filter.toLowerCase()));
    const choice = (id, text) =>
      h(`button.choice${form.project === id ? '.on' : ''}`, { type: 'button', onclick: () => { form.project = id; paint(); } }, text);
    list.replaceChildren(...shown.slice(0, 8).map((p) => choice(p.id, projectLabel(p.id))), choice(null, 'Без проєкту'),
      h('button.choice.ghost', { type: 'button', onclick: () => { adding = !adding; paint(); } }, '+ Новий'));
    newRow.style.display = adding ? 'flex' : 'none';
  };
  const list = h('div.row.wrap');
  const client = h('input', { placeholder: 'Замовник' });
  const name = h('input', { placeholder: 'Проєкт (необов’язково)' });
  const add = async () => {
    try {
      form.project = await call('add_project', { client: client.value, name: name.value });
      ui.snap.projects = (await call('snapshot', { day: ui.snap.today })).projects;
      adding = false;
      paint();
    } catch (e) {
      console.error(e);
    }
  };
  const newRow = h('div.row', {
    onkeydown: (e) => { if (e.key === 'Enter') { e.preventDefault(); e.stopPropagation(); add(); } },
  }, client, name, h('button', { type: 'button', onclick: add }, 'Додати'));
  box.append(
    h('div.row', {}, h('label.grow', {}, 'Проєкт / замовник'),
      ui.snap.projects.length > 5 && h('input', { placeholder: 'Пошук', style: 'width:140px', oninput: (e) => { filter = e.target.value; paint(); } })),
    list, newRow);
  paint();
  return box;
}

function taskForm(title, form, submitText, onsubmit, extra) {
  const save = async (e) => {
    e.preventDefault();
    try {
      await onsubmit(form);
    } catch (err) {
      console.error(err);
    }
  };
  return [
    drag(h('div.row', {}, h('h1', {}, title))),
    h('form.col.grow', { style: 'gap:10px;min-height:0', onsubmit: save, onkeydown: (e) => { if (e.key === 'Enter' && e.ctrlKey) save(e); } },
      h('label.col', {}, 'Заголовок', h('input', { value: form.title, 'data-focus': '', oninput: (e) => (form.title = e.target.value) })),
      h('label.col', {}, 'Текст', h('textarea', { value: form.body, rows: 3, oninput: (e) => (form.body = e.target.value) })),
      projectPicker(form),
      h('span.grow'),
      h('div.row', {}, extra, h('span.grow'),
        h('button.ghost', { type: 'button', onclick: escape }, 'Esc'),
        h('button.primary', { type: 'submit' }, submitText))),
  ];
}

const views = {
  tasks() {
    const tabs = ui.snap.tabs.map((t) => {
      const key = t.day ?? 'later';
      return h(`button${ui.day === key ? '.on' : ''}`, { onclick: () => { ui.day = key; refresh().catch(console.error); } },
        `${dayShort(t.day)} · ${t.open}`);
    });
    const input = h('input', {
      placeholder: 'Нова задача…',
      'data-focus': '',
      onkeydown: async (e) => {
        if (e.key !== 'Enter' || !input.value.trim()) return;
        const last = ui.snap.projects[0];
        await act(() => call('add_task', {
          title: input.value, body: '', project: last ? last.id : null, day: ui.day === 'later' ? null : ui.day,
        }))();
      },
    });
    return [
      header(),
      h('div.days', {}, tabs),
      h('div.scroll', {}, ui.snap.tasks.map(taskRow), !ui.snap.tasks.length && h('span.muted', {}, 'Тут поки нічого немає.')),
      h('div.row', {}, h('div.grow', {}, input),
        h('span.muted', {}, `${hotkeyLabel(ui.snap.settings.quick_hotkey)} з буфера`)),
      ui.snap.projects[0] && h('span.dim', {}, `Нова задача піде в «${projectLabel(ui.snap.projects[0].id)}», змінити можна в самій задачі.`),
    ];
  },

  quick() {
    const form = ui.draft;
    const days = h('div.row.wrap');
    const paint = () => days.replaceChildren(h('label', {}, 'День'), ...ui.snap.tabs.map((t) =>
      h(`button.choice${form.day === t.day ? '.on' : ''}`, { type: 'button', onclick: () => { form.day = t.day; paint(); } },
        t.day ? dayName(t.day) : 'Пізніше')));
    paint();
    return taskForm('Нова задача з буфера обміну', form, 'Створити · Enter', async (f) => {
      await call('add_task', { title: f.title, body: f.body, project: f.project, day: f.day });
      ui.draft = null;
      ui.view = 'tasks';
      await invoke('deactivate');
    }, days);
  },

  edit() {
    const t = ui.editing;
    const form = { title: t.title, body: t.body, project: t.project_id };
    const remove = h('button.ghost.bad', { type: 'button', onclick: () => {
      if (remove.dataset.sure) act(async () => { await call('delete_task', { id: t.id }); go('tasks'); })();
      remove.dataset.sure = '1';
      remove.textContent = 'Точно видалити?';
    } }, 'Видалити');
    return taskForm('Задача', form, 'Зберегти', async (f) => {
      await call('update_task', { id: t.id, title: f.title, body: f.body, project: f.project });
      go('tasks');
    }, remove);
  },

  plan() {
    const tasks = ui.snap.today_tasks.filter((t) => !t.done);
    const old = tasks.filter((t) => t.carried_from).length;
    return [
      header(),
      h('div.row', {}, h('h1.grow', {}, `План: ${dayLong(ui.snap.today)}`),
        h('span.muted', {}, `Задач: ${tasks.length}${old ? `, з них перенесено: ${old}` : ''}`)),
      h('div.scroll', {}, tasks.map(taskRow), !tasks.length && h('span.muted', {}, 'На сьогодні задач немає.')),
      h('div.row', {}, h('span.muted.grow', {}, 'Зайве перенеси на інший день: наведи на задачу й обери «на…».'),
        h('button.primary', { 'data-focus': '', onclick: act(async () => { await call('plan_seen'); ui.view = 'tasks'; }) }, 'Почати день')),
    ];
  },

  report() {
    const day = ui.reportDay ?? ui.snap.today;
    const body = h('div.scroll', {}, h('span.muted', {}, 'Завантаження…'));
    const total = h('span.muted');
    const generate = h('button', {}, 'Скласти');
    const copyAll = h('button.primary', {}, 'Копіювати все');
    let rows = [];
    const hours = (m) => `${String(m / 60).replace('.', ',')} год`;
    const sum = () => (total.textContent = `Разом ${hours(rows.reduce((a, r) => a + r.minutes, 0))}`);
    const paint = () => {
      sum();
      generate.textContent = rows.some((r) => r.memo) ? 'Перегенерувати' : 'Скласти';
      generate.disabled = copyAll.disabled = !rows.length;
      body.replaceChildren(...(rows.length ? rows.map(card) : [h('span.muted', {}, 'Немає закритих чи розпочатих задач.')]));
    };
    const card = (r) => {
      const time = h('span.hours', {}, hours(r.minutes));
      const step = (by) => async () => {
        r.minutes = Math.max(0, Math.min(1440, r.minutes + by));
        time.textContent = hours(r.minutes);
        sum();
        await call('save_minutes', { day, project: r.project_id, minutes: r.minutes }).catch(console.error);
      };
      const count = h('span.dim');
      const memo = h('textarea', { value: r.memo, rows: 2, lang: 'en', placeholder: r.tasks.map((t) => t.title).join('; ') });
      const counter = () => {
        const n = [...memo.value].length;
        count.textContent = `${n} / 140`;
        count.className = n > 140 ? 'dim bad' : 'dim';
      };
      memo.oninput = () => { r.memo = memo.value; counter(); };
      memo.onchange = () => call('save_memo', { day, project: r.project_id, memo: memo.value }).catch(console.error);
      counter();
      return h('div.card', {},
        h('div.row', {}, h('b.grow.clip', {}, r.label),
          h('button.step', { onclick: step(-30), ariaLabel: 'Менше часу' }, '−'), time,
          h('button.step', { onclick: step(30), ariaLabel: 'Більше часу' }, '+'),
          h('button', { onclick: () => copy(memo.value) }, 'Копіювати')),
        memo, h('div.row', {}, h('span.dim.grow.clip', {}, r.tasks.map((t) => t.title).join(' · ')), count));
    };
    const copy = async (text) => {
      if (!text) return toast('Запис порожній');
      await call('copy_text', { text }).then(() => toast('Скопійовано', true)).catch(console.error);
    };
    copyAll.onclick = () => copy(rows.filter((r) => r.memo).map((r) => r.memo).join('\n'));
    generate.onclick = async () => {
      generate.disabled = true;
      generate.textContent = 'Складаю…';
      try {
        const result = await call('generate_report', { day });
        rows = result.rows;
        if (!result.by_claude) toast('Ключа Claude API немає: записи складено з назв задач. Ключ додається в налаштуваннях.', true);
      } catch (e) {
        console.error(e);
      }
      paint();
    };
    call('report', { day }).then((r) => { rows = r; paint(); }).catch(console.error);
    const shift = (n) => () => { ui.reportDay = addDays(day, n); draw(); };
    return [
      header(),
      h('div.row', {}, h('button.ghost', { onclick: shift(-1), ariaLabel: 'Попередній день' }, '‹'),
        h('h1', {}, `Звіт: ${dayLong(day)}`),
        h('button.ghost', { onclick: shift(1), disabled: day >= ui.snap.today, ariaLabel: 'Наступний день' }, '›'),
        h('span.grow'), total),
      body,
      h('div.row', {}, h('span.muted.grow', {}, 'Час розподіляєш сам, текст можна правити.'), generate, copyAll),
    ];
  },

  meeting() {
    const s = meeting.status;
    const toggle = h(s.running ? 'button' : 'button.primary', {
      'data-focus': '',
      onclick: act(async () => {
        toggle.disabled = true;
        if (s.running) await call('stop_meeting');
        else {
          meeting.lines = [];
          meeting.partial = null;
          await call('start_meeting');
        }
      }),
    }, s.running ? 'Завершити' : 'Почати мітинг');
    return [
      header(),
      h('div.row', {}, h('h1', {}, 'Мітинг: субтитри EN → UA і пауза UA → EN'), h('span.grow'), toggle),
      meeting.status.note && h('div.row', {}, meetingNote()),
      s.running ? subtitles(200)
        : h('div.col.grow', {}, h('span.muted', {},
          'Асистент слухає звук із пристрою виводу за замовчуванням, тобто те, що кажуть інші в дзвінку, і показує переклад. ' +
          'Субтитри видно й тоді, коли вікно неактивне. Запуск триває кілька секунд: моделі завантажуються у відеокарту.')),
      pauseCard(),
      h('div.row', {}, ui.snap.meeting.gate.on
        ? h('span.dim.grow', {}, `Тримай ${hotkeyLabel(ui.snap.settings.pause_hotkey)}: дзвінок тебе не чує, а асистент перекладає українську на англійську.`)
        : h('span.bad.grow', {}, ui.snap.meeting.gate.note)),
    ];
  },

  settings() {
    const s = { ...ui.snap.settings };
    const save = act(async () => { await call('save_settings', { settings: s }); toast('Збережено', true); });
    const hotkey = (field) => {
      const input = h('input', {
        value: hotkeyLabel(s[field]),
        readOnly: true,
        onfocus: () => (input.value = 'Натисни комбінацію…'),
        onblur: () => (input.value = hotkeyLabel(s[field])),
        onkeydown: (e) => {
          if (['Tab', 'Escape'].includes(e.key)) return;
          e.preventDefault();
          e.stopPropagation();
          if (['Control', 'Alt', 'Shift', 'Meta'].includes(e.key)) return;
          const mods = [e.ctrlKey && 'Ctrl', e.altKey && 'Alt', e.shiftKey && 'Shift'].filter(Boolean);
          const fkey = /^F\d+$/.test(e.code);
          if (!fkey && !e.ctrlKey && !e.altKey) return toast('Потрібен Ctrl або Alt, або клавіша F1–F24');
          s[field] = [...mods, e.code].join('+');
          input.blur();
          save();
        },
      });
      return input;
    };
    // Device lists come from Windows on each visit to the settings.
    const select = (field, defaultLabel) => h('select', { onchange: (e) => { s[field] = e.target.value; save(); } },
      h('option', { value: s[field] }, s[field] || defaultLabel));
    const devices = { mic: select('mic_device', 'За замовчуванням'), cable: select('cable_device', 'CABLE Input') };
    call('audio_devices').then((d) => {
      const fill = (el, names, field, first) => el.replaceChildren(
        ...(first ? [h('option', { value: '', selected: !s[field] }, first)] : []),
        ...names.map((n) => h('option', { value: n, selected: s[field] && n.includes(s[field]) }, n)));
      fill(devices.mic, d.inputs.filter((n) => !/cable/i.test(n)), 'mic_device', 'За замовчуванням');
      fill(devices.cable, d.outputs.filter((n) => /cable/i.test(n)), 'cable_device');
      if (!devices.cable.options.length) devices.cable.append(h('option', { value: s.cable_device }, 'VB-Cable не знайдено'));
    }).catch(console.error);
    const keyInput = h('input', { type: 'password', placeholder: 'sk-ant-…', autocomplete: 'off' });
    const keyState = h('span.muted');
    call('has_api_key').then((yes) => (keyState.textContent = yes ? 'Ключ є.' : 'Ключа немає, звіт складатиметься з назв задач.')).catch(console.error);
    const opacity = h('input', {
      type: 'range', min: 20, max: 100, value: s.opacity,
      oninput: () => document.documentElement.style.setProperty('--opacity', opacity.value / 100),
      onchange: () => { s.opacity = Number(opacity.value); save(); },
    });
    return [
      header(),
      h('div.scroll', { style: 'gap:10px' },
        h('div.row', {}, h('label.col.grow', {}, 'Активувати вікно', hotkey('toggle_hotkey')),
          h('label.col.grow', {}, 'Задача з буфера', hotkey('quick_hotkey'))),
        h('label.col', {}, 'Непрозорість неактивного вікна', opacity),
        h('label.col', {}, 'Папка з моделями для мітингів',
          h('input', { value: s.models_dir, placeholder: ui.snap.meeting.default_models_dir,
            onchange: (e) => { s.models_dir = e.target.value; save(); } }),
          h('span.dim', {}, 'Потрібні ggml-large-v3-turbo.bin, ggml-silero-v6.2.0.bin і папки opus-mt-tc-big-en-zle, opus-mt-tc-big-zle-en.')),
        h('div.col', {}, h('label', {}, 'Режим паузи'),
          h('div.row', {}, h('label.col.grow', {}, 'Тримати, щоб дзвінок не чув', hotkey('pause_hotkey')),
            h('label.col.grow', {}, 'Мікрофон', devices.mic), h('label.col.grow', {}, 'Віртуальний кабель', devices.cable)),
          h(ui.snap.meeting.gate.on ? 'span.dim' : 'span.bad', {}, ui.snap.meeting.gate.on
            ? `Звук іде: ${ui.snap.meeting.gate.note}. У програмі для дзвінків вибери мікрофон «CABLE Output».`
            : ui.snap.meeting.gate.note)),
        h('div.col', {}, h('label', {}, 'Ключ Claude API (зберігається у сховищі облікових даних Windows)'),
          h('div.row', {}, keyInput, h('button', { onclick: act(async () => {
            await call('set_api_key', { key: keyInput.value });
            toast(keyInput.value.trim() ? 'Ключ збережено' : 'Ключ видалено', true);
          }) }, 'Зберегти')), keyState),
        h('div.col', {}, h('label', {}, 'Проєкти'),
          ui.snap.projects.map((p) => h('div.row', {}, h('span.grow.clip', {}, projectLabel(p.id)),
            h('button.ghost', { onclick: act(() => call('archive_project', { id: p.id })) }, 'В архів'))),
          !ui.snap.projects.length && h('span.muted', {}, 'Проєкти додаються під час створення задачі.'))),
      h('div.row', {}, h('span.grow'), h('button.ghost', { onclick: () => invoke('quit') }, 'Вийти з Sidekick')),
    ];
  },
};

function escape() {
  if (['quick', 'edit', 'settings'].includes(ui.view)) {
    const wasQuick = ui.view === 'quick';
    ui.draft = null;
    go('tasks');
    if (!wasQuick) return;
  }
  invoke('deactivate').catch(console.error);
}

document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') escape();
});

// Only views that show subtitles are redrawn: elsewhere a redraw would drop what is being typed.
const showsSubtitles = () => !ui.active || ui.view === 'meeting';

listen('subtitle', ({ payload }) => {
  if (payload.type === 'partial') meeting.partial = { en: payload.en, uk: payload.uk };
  else {
    meeting.partial = null;
    if (payload.en) meeting.lines.push({ en: payload.en, uk: payload.uk });
  }
  if (ui.snap && showsSubtitles()) draw();
});

listen('pause', ({ payload }) => {
  pause.held = payload.held;
  pause.meeting = payload.meeting;
  if (payload.held) pause.partial = null;
  if (ui.snap && showsSubtitles()) draw();
});

listen('said', ({ payload }) => {
  const said = { uk: payload.uk, en: payload.en };
  if (payload.partial) {
    if (pause.held) pause.partial = said;
  } else {
    pause.partial = null;
    pause.last = { ...said, at: Date.now() };
    if (payload.uk) meeting.lines.push({ me: true, ...said });
    // Hide the card once it has been on screen long enough.
    setTimeout(() => { if (ui.snap && showsSubtitles()) draw(); }, PAUSE_SHOWN_MS + 100);
  }
  if (ui.snap && showsSubtitles()) draw();
});

listen('meeting_status', ({ payload }) => {
  meeting.status = payload;
  if (!payload.running) meeting.partial = null;
  if (ui.snap && showsSubtitles()) draw();
});

listen('active', async ({ payload }) => {
  ui.active = payload.active;
  if (payload.active && payload.view === 'quick') {
    const draft = await call('clipboard_draft').catch(() => ({ title: '', body: '' }));
    await refresh().catch(console.error);
    const last = ui.snap.projects[0];
    ui.draft = { ...draft, project: last ? last.id : null, day: ui.snap.today };
    ui.view = 'quick';
    return draw();
  }
  if (payload.view) ui.view = payload.view;
  else if (payload.active && !ui.editing && ui.view !== 'quick') {
    ui.reportDay = null;
    if (ui.snap && ui.snap.plan_due) ui.view = 'plan';
  }
  refresh().catch(console.error);
});

// Midnight: tasks carry over and the summary changes even if nobody touches the window.
setInterval(() => { if (!ui.active) refresh().catch(console.error); }, 60_000);
refresh().catch(console.error);
