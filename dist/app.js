// Drives the desktop dashboard, two-tier break schedule settings, and pairing view.
const { invoke } = window.__TAURI__.core;

const tabBtnSchedule = document.getElementById("tab-btn-schedule");
const tabBtnPair = document.getElementById("tab-btn-pair");
const viewSchedule = document.getElementById("view-schedule");
const viewPair = document.getElementById("view-pair");

const statusEl = document.getElementById("status");
const scheduleFeedbackEl = document.getElementById("schedule-feedback");
const saveBadgeEl = document.getElementById("save-badge");
const statePill = document.getElementById("state-pill");
const stateLabel = document.getElementById("state-label");

const shortSub = document.getElementById("short-Sub");
const shortMeta = document.getElementById("short-meta");
const shortFill = document.getElementById("short-fill");
const longSub = document.getElementById("long-sub");
const longMeta = document.getElementById("long-meta");
const longFill = document.getElementById("long-fill");

const callNote = document.getElementById("call-note");
const pauseBtn = document.getElementById("toggle-pause");
const stretchBtn = document.getElementById("mark-stretch");
const testBtn = document.getElementById("test-buzz");

const shortEverySelect = document.getElementById("short-every");
const shortDurationSelect = document.getElementById("short-duration");
const longEverySelect = document.getElementById("long-every");
const longDurationSelect = document.getElementById("long-duration");

function switchTab(tab) {
  const isSchedule = tab === "schedule";
  tabBtnSchedule.classList.toggle("active", isSchedule);
  tabBtnSchedule.setAttribute("aria-selected", String(isSchedule));
  tabBtnPair.classList.toggle("active", !isSchedule);
  tabBtnPair.setAttribute("aria-selected", String(!isSchedule));
  viewSchedule.hidden = !isSchedule;
  viewPair.hidden = isSchedule;
}

function showScheduleFeedback(msg) {
  scheduleFeedbackEl.textContent = msg;
}

function showPairFeedback(msg) {
  statusEl.textContent = msg;
}

function ensureOption(selectEl, val) {
  const str = String(val);
  if (![...selectEl.options].some((o) => o.value === str)) {
    const opt = document.createElement("option");
    opt.value = str;
    opt.textContent = `${str}`;
    selectEl.appendChild(opt);
  }
  selectEl.value = str;
}

async function loadSettingsIntoForm() {
  const s = await invoke("get_settings");
  ensureOption(shortEverySelect, s.short_every_mins);
  ensureOption(shortDurationSelect, s.short_break_secs);
  ensureOption(longEverySelect, s.long_every_mins);
  ensureOption(longDurationSelect, s.long_break_mins);
}

async function saveScheduleFromForm() {
  const shortEveryMins = Number(shortEverySelect.value);
  const shortBreakSecs = Number(shortDurationSelect.value);
  const longEveryMins = Number(longEverySelect.value);
  const longBreakMins = Number(longDurationSelect.value);

  const saved = await invoke("save_settings", {
    shortEveryMins,
    shortBreakSecs,
    longEveryMins,
    longBreakMins,
  });
  ensureOption(shortEverySelect, saved.short_every_mins);
  ensureOption(shortDurationSelect, saved.short_break_secs);
  ensureOption(longEverySelect, saved.long_every_mins);
  ensureOption(longDurationSelect, saved.long_break_mins);
  saveBadgeEl.textContent = "Schedule saved";
  showScheduleFeedback(
    `Active: ${saved.short_every_mins}m quick stretch (${saved.short_break_secs}s) · ${saved.long_every_mins}m long break (${saved.long_break_mins}m).`
  );
  await refreshStatus();
}

async function refreshStatus() {
  try {
    const s = await invoke("app_status");
    const shortTargetMin = Math.max(1, Math.round(s.break_after_secs / 60));
    const shortPct = Math.min(100, Math.round((s.sitting_secs / Math.max(1, s.break_after_secs)) * 100));
    shortFill.style.width = `${shortPct}%`;
    shortSub.textContent = `· ${s.short_break_secs}s stretch, eyes & water`;

    if (s.break_after_secs < 120) {
      shortMeta.textContent = `${s.sitting_secs} / ${s.break_after_secs} s`;
    } else {
      shortMeta.textContent = `${s.sitting_minutes} / ${shortTargetMin} min`;
    }

    const longTargetMin = Math.max(1, Math.round(s.long_break_after_secs / 60));
    const longPct = Math.min(100, Math.round((s.long_sitting_secs / Math.max(1, s.long_break_after_secs)) * 100));
    longFill.style.width = `${longPct}%`;
    longSub.textContent = `· ${s.long_break_mins} min walk & back recovery`;
    longMeta.textContent = `${s.long_sitting_minutes} / ${longTargetMin} min`;

    if (s.paused) {
      statePill.className = "pill pill-paused";
      stateLabel.textContent = "Paused";
      pauseBtn.textContent = "Resume";
      callNote.hidden = true;
    } else if (s.call_app) {
      statePill.className = "pill pill-hold";
      stateLabel.textContent = `In call · ${s.call_app}`;
      pauseBtn.textContent = "Pause";
      callNote.hidden = false;
      callNote.textContent = `Mic in use by ${s.call_app} — buzzes are held until 30 s after the call ends.`;
    } else {
      statePill.className = "pill pill-active";
      stateLabel.textContent = "Active · Context-aware";
      pauseBtn.textContent = "Pause";
      callNote.hidden = true;
    }
  } catch (_) {}
}

async function render() {
  tabBtnSchedule.addEventListener("click", () => switchTab("schedule"));
  tabBtnPair.addEventListener("click", () => switchTab("pair"));

  const info = await invoke("pairing_info");
  // The SVG is generated by our own Rust code, never from user or network input.
  document.getElementById("qr").innerHTML = info.qr_svg;
  document.getElementById("code").value = info.code;

  if (info.first_run) {
    switchTab("pair");
  }

  document.getElementById("copy").addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(info.code);
      showPairFeedback("Pairing code copied to clipboard.");
    } catch (_) {
      showPairFeedback("Select the code above and press Ctrl+C.");
    }
  });

  testBtn.addEventListener("click", async () => {
    await invoke("send_test_buzz");
    showScheduleFeedback("Test buzz sent — check your phone.");
  });

  stretchBtn.addEventListener("click", async () => {
    await invoke("mark_quick_stretch");
    showScheduleFeedback("Quick stretch reset — 2-hour long break timer continues.");
    await refreshStatus();
  });

  pauseBtn.addEventListener("click", async () => {
    await invoke("toggle_pause");
    await refreshStatus();
  });

  for (const el of [shortEverySelect, shortDurationSelect, longEverySelect, longDurationSelect]) {
    el.addEventListener("change", () => {
      saveScheduleFromForm().catch((err) => showScheduleFeedback("Save failed: " + err));
    });
  }

  await loadSettingsIntoForm();
  await refreshStatus();
  setInterval(refreshStatus, 2000);
  await invoke("pairing_rendered", { qrShown: document.querySelector("#qr svg") !== null });
}

render().catch((err) => {
  showScheduleFeedback("Couldn't initialize window: " + err);
});
