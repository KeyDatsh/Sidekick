'use strict';
// The page only draws state and calls commands; all rules live in the Rust core.
// User text always goes through textContent/value, never innerHTML.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const ui = { active: false, view: 'tasks', day: null, reportDay: null, editing: null, draft: null, snap: null };
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
    h('div.row.tabs', {}, tab('tasks', 'Задачі'), tab('report', 'Звіт'), tab('plan', 'План дня'), drag(h('span.grow')),
      h('button.ghost', { onclick: () => go('settings'), ariaLabel: 'Налаштування', title: 'Налаштування' }, '⚙'),
      h('button.ghost', { onclick: () => invoke('deactivate'), title: 'Сховати (Esc)' }, 'Esc')),
  );
}

function inactive() {
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
