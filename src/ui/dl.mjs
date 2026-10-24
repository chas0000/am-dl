// 服务端下载页面：解析链接 → 建任务 → 轮询进度。
// 与上游前端零耦合：只依赖 /api/dl/*，上游更新前端不会影响这里。

const $ = (id) => document.getElementById(id);
const state = {
  preview: null, jobs: [], open: new Set(), picked: new Set(), token: '',
  // job.id → { ver, html }：子项内容没变就不重绘，避免 3 秒一次的闪烁与滚动跳动
  items: new Map(),
  // 只看失败的任务
  onlyFailed: new Set(),
};

/* ---------- 请求 ---------- */

function tokenParam() {
  return state.token ? `token=${encodeURIComponent(state.token)}` : '';
}

async function api(path, { method = 'GET', body, raw = false } = {}) {
  const sep = path.includes('?') ? '&' : '?';
  const url = tokenParam() ? `${path}${sep}${tokenParam()}` : path;
  const res = await fetch(url, {
    method,
    headers: body ? { 'content-type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  });
  if (res.status === 401) {
    const t = prompt('该服务器需要访问令牌：');
    if (t) {
      state.token = t.trim();
      localStorage.setItem('amdl-token', state.token);
      return api(path, { method, body, raw });
    }
    throw new Error('需要访问令牌');
  }
  const json = await res.json().catch(() => ({ code: 1, msg: `HTTP ${res.status}` }));
  if (json.code !== 0) throw new Error(json.msg || `HTTP ${res.status}`);
  return raw ? json : json.data;
}

/* ---------- 工具 ---------- */

const fmtBytes = (n) => {
  if (!n) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i ? 1 : 0)} ${units[i]}`;
};

const STATUS_TEXT = {
  queued: '排队中', running: '下载中', done: '完成', failed: '有失败', skipped: '已跳过', canceled: '已取消',
};
const ITEM_TEXT = {
  queued: '排队', running: '下载中', done: '完成', failed: '失败', skipped: '已存在', canceled: '取消',
};

function msg(text, kind = '') {
  const el = $('msg');
  el.textContent = text;
  el.className = `msg ${kind}`;
  if (text) setTimeout(() => { if (el.textContent === text) el.textContent = ''; }, 6000);
}

/* ---------- 解析 ---------- */

async function doPreview() {
  const url = $('url').value.trim();
  if (!url) return msg('先粘贴一个 Apple Music 链接', 'warn');
  $('preview').disabled = true;
  $('start').disabled = true;
  msg('解析中…');
  try {
    const data = await api('/api/dl/preview', { method: 'POST', body: { url } });
    state.preview = data;
    renderPreview(data);
    $('start').disabled = false;
    msg('');
  } catch (e) {
    state.preview = null;
    $('previewBox').classList.add('hidden');
    msg(e.message, 'error');
  } finally {
    $('preview').disabled = false;
  }
}

function renderPreview(data) {
  const box = $('previewBox');
  const kind = { album: '专辑', playlist: '歌单', song: '单曲' }[data.kind] || data.kind;
  box.innerHTML = `
    <div class="pv-head"><b>${escapeHtml(data.title)}</b>
      <small>${kind} · ${escapeHtml(data.artist || '')} · ${data.count} 首</small></div>
    <ol class="pv-list">${data.tracks.slice(0, 50).map((t) => `
      <li><span class="no">${t.disc_no > 1 ? `${t.disc_no}-` : ''}${t.track_no || ''}</span>
      <span class="t">${escapeHtml(t.title)}</span>
      <span class="a">${escapeHtml(t.artist)}</span></li>`).join('')}
      ${data.tracks.length > 50 ? `<li class="more">… 其余 ${data.tracks.length - 50} 首</li>` : ''}
    </ol>`;
  box.classList.remove('hidden');
}

function escapeHtml(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

/* ---------- 建任务 ---------- */

async function start() {
  const url = $('url').value.trim();
  if (!url) return msg('先粘贴链接', 'warn');
  $('start').disabled = true;
  try {
    const root = $('rootSel').value || '';
    const sub = $('destSub').value.trim().replace(/^\/+|\/+$/g, '');
    const dest = root ? (sub ? `${root}/${sub}` : root) : sub;
    const body = {
      url,
      dest,
      quality: $('quality').value,
      max_sample_rate: Number($('rate').value) || 0,
      album_dir: $('albumDir').value,
      track_file: $('trackFile').value,
      write_tags: $('tags').checked,
      embed_cover: $('cover').checked,
      lyrics: $('lyrics').value,
      syllable: $('syllable').checked,
      skip_existing: $('skip').checked,
    };
    const res = await api('/api/dl/jobs', { method: 'POST', body, raw: true });
    const job = res.data;
    const many = Array.isArray(res.jobs) ? res.jobs : [];
    if (many.length > 1) {
      const total = many.reduce((n, j) => n + (j.total || 0), 0);
      msg(`已加入队列：${many.length} 张专辑，共 ${total} 首`, 'ok');
      // 默认全部展开会让列表很长，只展开第一个
      state.open.add(many[0].id);
    } else {
      msg(`已创建任务：${job.title}（${job.items.length} 首）`, 'ok');
      state.open.add(job.id);
    }
    await refresh();
  } catch (e) {
    msg(e.message, 'error');
  } finally {
    $('start').disabled = false;
  }
}

/* ---------- 任务列表 ---------- */

async function refresh() {
  try {
    const list = await api('/api/dl/jobs');
    state.jobs = list;
    renderJobs(list);
  } catch (e) {
    msg(e.message, 'error');
  }
}

function renderJobs(list) {
  const host = $('jobs');
  const scrollY = window.scrollY;
  // 每次重绘都会重建 DOM，先把各任务子项列表的滚动位置记下来
  const innerScroll = new Map();
  for (const n of host.querySelectorAll('.job')) {
    const it = n.querySelector('.items');
    if (it && n.dataset.id) innerScroll.set(n.dataset.id, it.scrollTop);
  }
  host.innerHTML = '';
  if (!list.length) {
    host.innerHTML = '<p class="empty">还没有任务</p>';
    return;
  }
  for (const job of list) {
    const node = $('jobTpl').content.firstElementChild.cloneNode(true);
    node.dataset.id = job.id;
    const box = node.querySelector('.pickBox');
    box.checked = state.picked.has(job.id);
    box.onchange = () => {
      if (box.checked) state.picked.add(job.id); else state.picked.delete(job.id);
      updatePickInfo();
    };
    // 点复选框不要触发卡片上的其它操作
    node.querySelector('.pick').onclick = (e) => e.stopPropagation();
    node.querySelector('.job-title b').textContent = job.title || '(未命名)';
    node.querySelector('.job-title small').textContent = [job.artist, `${job.done}/${job.total} 首`, fmtBytes(job.bytes)].filter(Boolean).join(' · ');
    const badge = node.querySelector('.job-badge');
    badge.textContent = STATUS_TEXT[job.status] || job.status;
    badge.className = `job-badge ${job.status}`;
    const pct = job.total ? Math.round(((job.done + job.failed) / job.total) * 100) : 0;
    node.querySelector('.bar i').style.width = `${pct}%`;
    node.querySelector('.job-sub').textContent = job.failed ? `${job.failed} 首失败` : `${pct}%`;

    const items = node.querySelector('.items');
    const keepScroll = innerScroll.get(job.id) || 0;
    if (state.open.has(job.id)) {
      items.classList.remove('hidden');
      const cached = state.items.get(job.id);
      const done = ['done', 'failed', 'canceled', 'skipped'].includes(job.status);

      if (cached) {
        items.innerHTML = cached.html;
        applyFilter(items, job.id);
        // 已完成的任务内容不会再变；进行中的只在版本变了才重新拉
        if (!done && cached.ver !== job.updated_at) loadItems(job.id, items, job.updated_at, keepScroll);
      } else {
        items.innerHTML = '<li class="item running"><span class="st">加载中…</span></li>';
        loadItems(job.id, items, job.updated_at, keepScroll);
      }
    }
    node.querySelector('.act-toggle').onclick = () => {
      if (state.open.has(job.id)) state.open.delete(job.id); else state.open.add(job.id);
      renderJobs(state.jobs);
    };
    node.querySelector('.act-retry').onclick = async () => {
      try { await api(`/api/dl/jobs/${job.id}/retry`, { method: 'POST' }); await refresh(); }
      catch (e) { msg(e.message, 'error'); }
    };
    node.querySelector('.act-cancel').onclick = async () => {
      try { await api(`/api/dl/jobs/${job.id}/cancel`, { method: 'POST' }); await refresh(); }
      catch (e) { msg(e.message, 'error'); }
    };
    node.querySelector('.act-remove').onclick = async () => {
      if (!confirm('移除这个任务记录？（已下载的文件不会删除）')) return;
      try { await api(`/api/dl/jobs/${job.id}/remove`, { method: 'POST' }); state.open.delete(job.id); await refresh(); }
      catch (e) { msg(e.message, 'error'); }
    };
    host.appendChild(node);
    // 必须挂到文档上之后再恢复滚动位置：游离节点的 scrollTop 赋值无效
    if (state.open.has(job.id)) items.scrollTop = keepScroll;
  }
  // 已不存在的任务从选择集里剔掉
  const ids = new Set(list.map((j) => j.id));
  for (const id of [...state.picked]) if (!ids.has(id)) state.picked.delete(id);
  for (const id of [...state.items.keys()]) if (!ids.has(id)) state.items.delete(id);
  updatePickInfo();
  if (window.scrollY !== scrollY) window.scrollTo(0, scrollY);
}

function updatePickInfo() {
  const total = state.jobs.length;
  const n = state.picked.size;
  $('pickInfo').textContent = n ? `已选 ${n} / ${total}` : `共 ${total} 个任务`;
  $('pickAll').checked = total > 0 && n === total;
  $('pickAll').indeterminate = n > 0 && n < total;
  $('bulkRetry').disabled = n === 0;
  $('bulkRemove').disabled = n === 0;
  $('bulkClean').disabled = !state.jobs.some((j) => j.status === 'done');
}

async function batch(action, body, okText) {
  try {
    const res = await api('/api/dl/jobs/batch', { method: 'POST', body: { action, ...body } });
    msg(`${okText}（${res.affected} 个）`, 'ok');
    if (action === 'remove') {
      for (const id of body.ids || []) state.picked.delete(id);
      if (!body.ids) state.picked.clear();
    }
    await refresh();
  } catch (e) {
    msg(e.message, 'error');
  }
}

// 「只看失败」开关：给容器加 class，由 CSS 隐藏其它行
function applyFilter(host, id) {
  host.classList.toggle('only-failed', state.onlyFailed.has(id));
  const cb = host.querySelector('.onlyFailed');
  if (cb) {
    cb.checked = state.onlyFailed.has(id);
    cb.onchange = () => {
      if (cb.checked) state.onlyFailed.add(id); else state.onlyFailed.delete(id);
      applyFilter(host, id);
    };
  }
}

async function loadItems(id, host, ver, keepScroll = 0) {
  try {
    const job = await api(`/api/dl/jobs/${id}`);
    const scrollTop = keepScroll || host.scrollTop;
    const failed = job.items.filter((it) => it.status === 'failed').length;
    const bar = failed
      ? `<li class="item-filter"><label><input type="checkbox" class="onlyFailed"> 只看失败（${failed}）</label></li>`
      : '';
    const html = bar + job.items.map((it) => `
      <li class="item ${it.status}">
        <span class="st">${ITEM_TEXT[it.status] || it.status}</span>
        <span class="t">${escapeHtml(it.spec.title)}</span>
        <span class="q">${escapeHtml(it.quality || '')}</span>
        <span class="sz">${it.bytes ? fmtBytes(it.bytes) : ''}</span>
        ${it.error ? `<span class="er" title="${escapeHtml(it.error)}">${escapeHtml(it.error.slice(0, 80))}</span>` : ''}
      </li>`).join('');
    state.items.set(id, { ver, html });
    host.innerHTML = html;
    host.scrollTop = scrollTop;
    applyFilter(host, id);
  } catch (e) {
    const html = `<li class="item failed"><span class="er">${escapeHtml(e.message)}</span></li>`;
    host.innerHTML = html;
  }
}

/* ---------- 启动 ---------- */

async function boot() {
  state.token = localStorage.getItem('amdl-token') || '';
  // 支持 /dl?token=xxx 直接打开（远程第一次访问时省得手输）
  const fromUrl = new URLSearchParams(location.search).get('token');
  if (fromUrl) {
    state.token = fromUrl.trim();
    localStorage.setItem('amdl-token', state.token);
    history.replaceState(null, '', location.pathname);
  }
  try {
    const cfg = await api('/api/dl/config');
    $('root').textContent = cfg.root;
    $('free').textContent = `剩余 ${fmtBytes(cfg.free_bytes)} · 并发 ${cfg.concurrency}`;
    const sel = $('rootSel');
    sel.innerHTML = (cfg.roots || [{ path: cfg.root, free_bytes: cfg.free_bytes }])
      .map((r) => `<option value="${escapeHtml(r.path)}">${escapeHtml(r.path)} · 剩 ${fmtBytes(r.free_bytes)}</option>`)
      .join('');
    // 只有一个根目录时藏起来，少一行干扰
    if ((cfg.roots || []).length <= 1) sel.closest('.grid2').classList.add('single');
    if (cfg.token_required && !state.token) msg('该服务器需要访问令牌，请点「解析链接」时输入', 'warn');
  } catch (e) {
    msg(e.message, 'error');
  }
  $('pickAll').onchange = () => {
    if ($('pickAll').checked) state.jobs.forEach((j) => state.picked.add(j.id));
    else state.picked.clear();
    renderJobs(state.jobs);
  };
  $('bulkRetry').onclick = () => {
    if (!state.picked.size) return;
    batch('retry', { ids: [...state.picked] }, '已重新排队');
  };
  $('bulkRemove').onclick = () => {
    if (!state.picked.size) return;
    if (!confirm(`删除 ${state.picked.size} 个任务记录？（已下载的文件不会删除）`)) return;
    batch('remove', { ids: [...state.picked] }, '已删除记录');
  };
  $('bulkClean').onclick = () => {
    const done = state.jobs.filter((j) => j.status === 'done').length;
    if (!done) return msg('没有已完成的任务', 'warn');
    if (!confirm(`删除 ${done} 条已完成任务的记录？（文件保留）`)) return;
    batch('remove', { filter: 'done' }, '已清理');
  };
  $('preview').onclick = doPreview;
  $('start').onclick = start;
  $('refresh').onclick = refresh;
  $('url').addEventListener('keydown', (e) => { if (e.key === 'Enter') doPreview(); });
  await refresh();
  setInterval(refresh, 3000);
}

boot();
