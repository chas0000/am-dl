// 注入到上游播放器页面的「下载到服务器」浮窗。
// 默认收成右下角一个小圆钮（半透明，几乎不挡内容）；悬停 / 点击展开成完整工具条，
// 鼠标离开、失焦或 2.5 秒无操作后自动收起。不碰上游 DOM 结构。

const MARK = 'data-amdl-bar';
const QUALITY_KEY = 'amdl-quality';
const TOKEN_KEY = 'amdl-token';
const IDLE_MS = 2500;

const QUALITIES = [
  ['best', '最佳'],
  ['alac', 'ALAC'],
  ['atmos', '全景声'],
  ['aac', 'AAC'],
];

/** 当前 SPA 路由对应的 Apple Music 链接；不是可下载页面时返回 '' */
function currentUrl() {
  const p = location.pathname;
  if (!p.startsWith('/https://music.apple.com/')) return '';
  const raw = decodeURIComponent(p.slice(1));
  if (!/\/(album|playlist|song|music-video|artist)\//.test(raw)) return '';
  return raw + (location.search || '');
}

function token() {
  const fromUrl = new URLSearchParams(location.search).get('token');
  if (fromUrl) {
    localStorage.setItem(TOKEN_KEY, fromUrl.trim());
    return fromUrl.trim();
  }
  return localStorage.getItem(TOKEN_KEY) || '';
}

function toast(text, link) {
  let el = document.querySelector('[data-amdl-toast]');
  if (!el) {
    el = document.createElement('div');
    el.setAttribute('data-amdl-toast', '');
    el.className = 'amdl-toast';
    document.body.appendChild(el);
  }
  el.innerHTML = '';
  el.append(text);
  if (link) {
    const a = document.createElement('a');
    a.href = link;
    a.target = '_blank';
    a.rel = 'noopener';
    a.textContent = '打开下载中心 ↗';
    el.append(' ', a);
  }
  el.classList.add('show');
  clearTimeout(el._t);
  el._t = setTimeout(() => el.classList.remove('show'), 6000);
}

const CSS = `
  .amdl-bar {
    position: fixed; right: 16px; bottom: 104px; z-index: 2147483000;
    display: flex; align-items: center; gap: 6px;
    max-width: calc(100vw - 32px);
    padding: 6px; border-radius: 14px;
    background: rgba(22,24,29,.94); border: 1px solid #2a2e37;
    box-shadow: 0 6px 24px rgba(0,0,0,.45); backdrop-filter: blur(8px);
    font: 13px/1 -apple-system, "PingFang SC", "Microsoft YaHei", sans-serif;
    opacity: .6; transition: opacity .18s, padding .18s, border-radius .18s;
  }
  .amdl-bar:hover, .amdl-bar:focus-within, .amdl-bar.expanded { opacity: 1; }

  /* 收起态：只剩一个小圆钮 */
  .amdl-bar.collapsed { padding: 0; border-radius: 999px; background: transparent; border-color: transparent; box-shadow: none; }
  .amdl-bar.collapsed .amdl-full { display: none; }
  .amdl-bar:not(.collapsed) .amdl-mini { display: none; }

  .amdl-mini {
    width: 42px; height: 42px; border-radius: 999px;
    display: flex; align-items: center; justify-content: center;
    background: rgba(250,45,72,.82); color: #fff; border: 0; cursor: pointer;
    font-size: 17px; line-height: 1; padding: 0;
    box-shadow: 0 4px 16px rgba(0,0,0,.4);
    transition: background .15s, transform .15s;
  }
  .amdl-mini:hover { background: #ff4159; transform: scale(1.06); }
  .amdl-mini.busy { background: #7a2230; }
  .amdl-mini.has-url::after {
    content: ''; position: absolute; width: 8px; height: 8px; border-radius: 50%;
    background: #3ecf8e; transform: translate(15px, -15px);
  }

  .amdl-full { display: flex; align-items: center; gap: 6px; }
  .amdl-bar select {
    flex: 0 0 auto;
    background: #1c1f26; color: #e7e9ee; border: 1px solid #2a2e37;
    border-radius: 8px; padding: 6px 8px; font: inherit; outline: none;
  }
  .amdl-bar input {
    flex: 1 1 150px; min-width: 0;
    background: #12141a; color: #e7e9ee; border: 1px solid #2a2e37;
    border-radius: 8px; padding: 6px 9px; font: inherit; outline: none;
  }
  .amdl-bar input:focus { border-color: #414855; }
  .amdl-bar input::placeholder { color: #6d7481; }
  .amdl-bar .amdl-go {
    flex: 0 0 auto;
    background: #fa2d48; color: #fff; border: 0; border-radius: 8px;
    padding: 7px 12px; font: inherit; font-weight: 600; cursor: pointer;
    white-space: nowrap;
  }
  .amdl-bar .amdl-go:disabled { opacity: .45; cursor: default; }
  .amdl-bar a {
    flex: 0 0 auto; color: #9aa1ad; text-decoration: none; padding: 0 4px; font-size: 15px;
  }
  .amdl-bar a:hover { color: #e7e9ee; }
  .amdl-bar.amdl-nudge { box-shadow: 0 0 0 2px rgba(250,45,72,.6), 0 6px 24px rgba(0,0,0,.45); }

  .amdl-toast {
    position: fixed; right: 16px; bottom: 160px; z-index: 2147483001;
    max-width: min(380px, calc(100vw - 32px)); padding: 10px 14px; border-radius: 10px;
    background: #1c1f26; border: 1px solid #2a2e37; color: #e7e9ee;
    font: 13px/1.4 -apple-system, "PingFang SC", "Microsoft YaHei", sans-serif;
    box-shadow: 0 8px 28px rgba(0,0,0,.5);
    opacity: 0; transform: translateY(8px); transition: .18s; pointer-events: none;
  }
  .amdl-toast.show { opacity: 1; transform: none; pointer-events: auto; }
  .amdl-toast a { color: #fa2d48; text-decoration: none; }

  @media (max-width: 560px) {
    .amdl-bar { left: 8px; right: 8px; bottom: 92px; }
    .amdl-bar.collapsed { left: auto; }
    .amdl-bar .amdl-full { flex-wrap: wrap; }
    .amdl-bar input { flex: 1 1 100%; order: 4; }
  }
`;

async function createJob(ui, url, quality) {
  const { bar, mini, btn } = ui;
  btn.disabled = true;
  mini.classList.add('busy');
  const old = btn.textContent;
  btn.textContent = '提交中…';
  try {
    const body = { url, quality };
    let res = await fetch(`/api/dl/jobs?token=${encodeURIComponent(token())}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    });
    if (res.status === 401) {
      const t = prompt('该服务器需要访问令牌：');
      if (!t) throw new Error('已取消');
      localStorage.setItem(TOKEN_KEY, t.trim());
      res = await fetch(`/api/dl/jobs?token=${encodeURIComponent(t.trim())}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
      });
    }
    const json = await res.json().catch(() => ({ code: 1, msg: `HTTP ${res.status}` }));
    if (json.code !== 0) throw new Error(json.msg || `HTTP ${res.status}`);
    const job = json.data;
    toast(`已加入队列：${job.title || url}（${job.items.length} 首）`, `/dl?token=${encodeURIComponent(token())}`);
    ui.collapse();
  } catch (e) {
    toast(`提交失败：${e.message}`);
  } finally {
    btn.disabled = false;
    btn.textContent = old;
    mini.classList.remove('busy');
  }
}

function mount() {
  if (document.querySelector(`[${MARK}]`)) return;

  const style = document.createElement('style');
  style.textContent = CSS;
  document.head.appendChild(style);

  const bar = document.createElement('div');
  bar.setAttribute(MARK, '');
  bar.className = 'amdl-bar collapsed';

  // —— 收起态的小圆钮 ——
  const mini = document.createElement('button');
  mini.type = 'button';
  mini.className = 'amdl-mini';
  mini.title = '下载到服务器';
  mini.setAttribute('aria-label', '下载到服务器');
  mini.textContent = '↓';

  // —— 展开后的工具条 ——
  const full = document.createElement('div');
  full.className = 'amdl-full';

  const select = document.createElement('select');
  select.title = '音质';
  for (const [value, label] of QUALITIES) {
    const opt = document.createElement('option');
    opt.value = value;
    opt.textContent = label;
    select.append(opt);
  }
  select.value = localStorage.getItem(QUALITY_KEY) || 'best';
  select.onchange = () => localStorage.setItem(QUALITY_KEY, select.value);

  const input = document.createElement('input');
  input.type = 'text';
  input.spellcheck = false;
  input.placeholder = '粘贴 Apple Music 链接';
  input.setAttribute('autocomplete', 'off');

  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'amdl-go';
  btn.textContent = '下载到服务器';
  btn.disabled = true;

  const link = document.createElement('a');
  link.href = `/dl?token=${encodeURIComponent(token())}`;
  link.target = '_blank';
  link.rel = 'noopener';
  link.title = '下载中心';
  link.textContent = '↗';

  let idleTimer;
  const isOpen = () => !bar.classList.contains('collapsed');

  function collapse() {
    if (!isOpen()) return;
    // 正在输入时不收，避免打字到一半浮窗跑掉
    const active = document.activeElement;
    if (active && bar.contains(active) && active !== mini) return;
    bar.classList.add('collapsed');
    bar.classList.remove('expanded');
    clearTimeout(idleTimer);
  }

  function expand() {
    if (!isOpen()) {
      bar.classList.remove('collapsed');
      bar.classList.add('expanded');
    }
    clearTimeout(idleTimer);
    idleTimer = setTimeout(() => {
      if (!bar.matches(':hover') && !bar.contains(document.activeElement)) collapse();
    }, IDLE_MS);
  }

  const ui = { bar, mini, btn, collapse };

  mini.addEventListener('click', (e) => {
    e.stopPropagation();
    expand();
    // 可下载页面已经自动填好链接，展开后直接让回车可用
    setTimeout(() => (input.value.trim() ? btn.focus() : input.focus()), 30);
  });
  bar.addEventListener('mouseenter', expand);
  bar.addEventListener('mouseleave', () => {
    clearTimeout(idleTimer);
    idleTimer = setTimeout(collapse, IDLE_MS);
  });
  bar.addEventListener('focusin', expand);
  bar.addEventListener('focusout', () => {
    clearTimeout(idleTimer);
    idleTimer = setTimeout(collapse, 400);
  });
  document.addEventListener(
    'pointerdown',
    (e) => {
      if (!bar.contains(e.target)) collapse();
    },
    true,
  );

  /* ---------- 提交 ---------- */
  const submit = () => {
    const url = input.value.trim();
    if (!url) {
      input.focus();
      bar.classList.add('amdl-nudge');
      setTimeout(() => bar.classList.remove('amdl-nudge'), 900);
      return;
    }
    createJob(ui, url, select.value);
  };

  input.addEventListener('input', () => {
    btn.disabled = !input.value.trim();
  });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      submit();
    }
    if (e.key === 'Escape') collapse();
  });
  btn.onclick = submit;

  full.append(select, input, btn, link);
  bar.append(mini, full);
  document.body.appendChild(bar);

  /* ---------- 跟随 SPA 路由 ---------- */
  let autoFilled = '';
  const sync = () => {
    const url = currentUrl();
    mini.classList.toggle('has-url', !!url);
    if (!url) return;
    const now = input.value.trim();
    if (now === '' || now === autoFilled) {
      input.value = url;
      autoFilled = url;
      btn.disabled = false;
    }
  };
  let last = null;
  setInterval(() => {
    const url = currentUrl();
    if (url !== last) {
      last = url;
      sync();
    }
  }, 400);
  window.addEventListener('popstate', sync);
  sync();
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', mount, { once: true });
} else {
  mount();
}
