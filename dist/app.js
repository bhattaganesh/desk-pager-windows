// Drives the responsive Bento dashboard, radial gauges, custom schedule studio, and phone pairing view.
const { invoke } = window.__TAURI__.core;

const RING_CIRCUMFERENCE = 2 * Math.PI * 50; // r = 50 -> 314.159

const tabBtnSchedule = document.getElementById("tab-btn-schedule");
const tabBtnPair = document.getElementById("tab-btn-pair");
const viewSchedule = document.getElementById("view-schedule");
const viewPair = document.getElementById("view-pair");
const desktopNotifyCheck = document.getElementById("desktop-notify-check");
const autostartCheck = document.getElementById("autostart-check");
const installBanner = document.getElementById("install-banner");
const dismissInstallBtn = document.getElementById("dismiss-install");

const statusEl = document.getElementById("status");
const scheduleFeedbackEl = document.getElementById("schedule-feedback");
const saveBadgeEl = document.getElementById("save-badge");
const statePill = document.getElementById("state-pill");
const stateLabel = document.getElementById("state-label");

const shortRing = document.getElementById("short-ring");
const shortTag = document.getElementById("short-tag");
const shortSub = document.getElementById("short-Sub");
const shortMeta = document.getElementById("short-meta");
const shortRem = document.getElementById("short-rem");

const longRing = document.getElementById("long-ring");
const longSub = document.getElementById("long-sub");
const longMeta = document.getElementById("long-meta");
const longRem = document.getElementById("long-rem");

const callNote = document.getElementById("call-note");
const pauseBtn = document.getElementById("toggle-pause");
const meetingBtn = document.getElementById("toggle-meeting");
const stretchBtn = document.getElementById("mark-stretch");
const resetAllBtn = document.getElementById("reset-all");
const testBtn = document.getElementById("test-buzz");
const regenBtn = document.getElementById("regenerate");

const shortEveryInput = document.getElementById("short-every");
const shortDurationInput = document.getElementById("short-duration");
const longEveryInput = document.getElementById("long-every");
const longDurationInput = document.getElementById("long-duration");

let regenConfirmPending = false;
let saveDebounce = null;

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

function setRingProgress(circleEl, ratio) {
  const clamped = Math.max(0, Math.min(1, ratio));
  const offset = RING_CIRCUMFERENCE * (1 - clamped);
  circleEl.style.strokeDashoffset = `${offset.toFixed(2)}`;
}

function syncPresetChips() {
  for (const group of document.querySelectorAll(".chips")) {
    const targetId = group.getAttribute("data-for");
    const input = document.getElementById(targetId);
    if (!input) continue;
    const currentVal = String(Number(input.value));
    for (const chip of group.querySelectorAll(".chip")) {
      chip.classList.toggle("active", chip.getAttribute("data-val") === currentVal);
    }
  }
}

async function loadSettingsIntoForm() {
  const s = await invoke("get_settings");
  shortEveryInput.value = s.short_every_mins;
  shortDurationInput.value = s.short_break_secs;
  longEveryInput.value = s.long_every_mins;
  longDurationInput.value = s.long_break_mins;
  desktopNotifyCheck.checked = Boolean(s.desktop_notify);
  syncPresetChips();
}

async function saveScheduleFromForm() {
  const shortEveryMins = Math.max(1, Math.round(Number(shortEveryInput.value) || 30));
  const shortBreakSecs = Math.max(5, Math.round(Number(shortDurationInput.value) || 30));
  const longEveryMins = Math.max(5, Math.round(Number(longEveryInput.value) || 120));
  const longBreakMins = Math.max(1, Math.round(Number(longDurationInput.value) || 10));

  const saved = await invoke("save_settings", {
    shortEveryMins,
    shortBreakSecs,
    longEveryMins,
    longBreakMins,
    desktopNotify: desktopNotifyCheck.checked,
  });
  shortEveryInput.value = saved.short_every_mins;
  shortDurationInput.value = saved.short_break_secs;
  longEveryInput.value = saved.long_every_mins;
  longDurationInput.value = saved.long_break_mins;
  desktopNotifyCheck.checked = Boolean(saved.desktop_notify);
  syncPresetChips();
  saveBadgeEl.textContent = "Saved live";
  showScheduleFeedback(
    `Active: Quick stretch every ${saved.short_every_mins}m (${saved.short_break_secs}s pause) · Long break every ${saved.long_every_mins}m (${saved.long_break_mins}m walk).`
  );
  await refreshStatus();
}

function scheduleAutoSave() {
  syncPresetChips();
  if (saveDebounce) clearTimeout(saveDebounce);
  saveDebounce = setTimeout(() => {
    saveScheduleFromForm().catch((err) => showScheduleFeedback("Save failed: " + err));
  }, 350);
}

async function refreshStatus() {
  try {
    const s = await invoke("app_status");
    autostartCheck.checked = Boolean(s.autostart);
    desktopNotifyCheck.checked = Boolean(s.desktop_notify);
    if (installBanner) {
      installBanner.hidden = !s.just_installed;
    }

    const shortTargetMin = Math.max(1, Math.round(s.break_after_secs / 60));
    shortTag.textContent =
      s.stretches_completed > 0
        ? `Tier 1 · Quick Stretch (${s.stretches_completed} done)`
        : "Tier 1 · Quick Stretch";
    shortSub.textContent = `${s.short_break_secs}s pause · spine, eyes & water`;

    if (s.stretch_pause_rem_secs > 0) {
      const pauseRatio = s.stretch_pause_rem_secs / Math.max(1, s.short_break_secs);
      setRingProgress(shortRing, pauseRatio);
      shortMeta.textContent = `${s.stretch_pause_rem_secs}s / ${s.short_break_secs}s`;
      shortRem.textContent = "Stretch pause…";
    } else if (s.break_after_secs < 120) {
      const shownSecs = Math.min(s.sitting_secs, s.break_after_secs);
      setRingProgress(shortRing, shownSecs / Math.max(1, s.break_after_secs));
      shortMeta.textContent = `${shownSecs}/${s.break_after_secs}s`;
      const remS = Math.max(0, s.break_after_secs - shownSecs);
      shortRem.textContent = remS === 0 ? "Stretch due!" : `${remS}s left`;
    } else {
      const shownShortMin = Math.min(s.sitting_minutes, shortTargetMin);
      setRingProgress(shortRing, Math.min(1, s.sitting_secs / Math.max(1, s.break_after_secs)));
      shortMeta.textContent = `${shownShortMin} / ${shortTargetMin}m`;
      const remM = Math.max(0, shortTargetMin - shownShortMin);
      shortRem.textContent = remM === 0 ? "Stretch due!" : `${remM}m left`;
    }

    const longTargetMin = Math.max(1, Math.round(s.long_break_after_secs / 60));
    const shownLongMin = Math.min(s.long_sitting_minutes, longTargetMin);
    const longRatio = Math.min(1, s.long_sitting_secs / Math.max(1, s.long_break_after_secs));
    setRingProgress(longRing, longRatio);
    longSub.textContent = `${s.long_break_mins} min walk & back recovery`;
    longMeta.textContent = `${shownLongMin} / ${longTargetMin}m`;
    const longRemM = Math.max(0, longTargetMin - shownLongMin);
    if (s.long_sitting_secs >= s.long_break_after_secs && s.idle_secs >= 10) {
      longRem.textContent = `Break ${Math.floor(s.idle_secs / 60)}/${s.long_break_mins}m`;
    } else {
      longRem.textContent = longRemM === 0 ? "Long break due!" : `${longRemM}m left`;
    }

    meetingBtn.textContent = s.in_meeting ? "In a Meeting: On" : "In a Meeting: Off";
    meetingBtn.classList.toggle("btn-meeting-on", Boolean(s.in_meeting));

    if (s.paused) {
      statePill.className = "pill pill-paused";
      stateLabel.textContent = "Paused";
      pauseBtn.textContent = "Resume Timer";
      callNote.hidden = true;
    } else if (s.call_app) {
      statePill.className = "pill pill-hold";
      stateLabel.textContent = `Hold · ${s.call_app}`;
      pauseBtn.textContent = "Pause Timer";
      callNote.hidden = false;
      callNote.textContent =
        s.call_app === "Meeting mode"
          ? "Meeting hold is active — sitting time is tracked, and buzzes wait until you turn Meeting mode off."
          : `Microphone in use by ${s.call_app} — buzzes are held until 30 s after the call ends.`;
    } else {
      statePill.className = "pill pill-active";
      stateLabel.textContent = "Active · Context-aware";
      pauseBtn.textContent = "Pause Timer";
      callNote.hidden = true;
    }
  } catch (_) {}
}

async function render() {
  tabBtnSchedule.addEventListener("click", () => switchTab("schedule"));
  tabBtnPair.addEventListener("click", () => switchTab("pair"));
  if (dismissInstallBtn) {
    dismissInstallBtn.addEventListener("click", async () => {
      installBanner.hidden = true;
      await invoke("dismiss_install_banner");
    });
  }

  const info = await invoke("pairing_info");
  // The SVG is generated by our own Rust code, never from user or network input.
  document.getElementById("qr").innerHTML = info.qr_svg;
  document.getElementById("code").value = info.code;

  if (info.first_run) {
    switchTab("pair");
  }

  document.getElementById("copy").addEventListener("click", async () => {
    const currentCode = document.getElementById("code").value;
    try {
      await navigator.clipboard.writeText(currentCode);
      showPairFeedback("Pairing code copied to clipboard.");
    } catch (_) {
      showPairFeedback("Select the code above and press Ctrl+C.");
    }
  });

  regenBtn.addEventListener("click", async () => {
    if (!regenConfirmPending) {
      regenConfirmPending = true;
      regenBtn.textContent = "Click again to confirm new code (unpairs current phone)";
      showPairFeedback("Confirming will generate a new 128-bit secret topic.");
      setTimeout(() => {
        regenConfirmPending = false;
        regenBtn.textContent = "Regenerate Secret Code";
      }, 5000);
      return;
    }
    regenConfirmPending = false;
    regenBtn.textContent = "Regenerate Secret Code";
    const updated = await invoke("regenerate_pairing");
    document.getElementById("qr").innerHTML = updated.qr_svg;
    document.getElementById("code").value = updated.code;
    showPairFeedback("New secret pairing code generated. Scan or paste it on your phone.");
  });

  desktopNotifyCheck.addEventListener("change", async () => {
    try {
      const enabled = await invoke("set_desktop_notify_cmd", { enable: desktopNotifyCheck.checked });
      desktopNotifyCheck.checked = Boolean(enabled);
      showScheduleFeedback(
        enabled
          ? "Silent PC notifications enabled (shows in Windows Notification Center alongside phone buzz)."
          : "Silent PC notifications disabled (mobile-only mode)."
      );
    } catch (err) {
      showScheduleFeedback("Could not update PC notification preference: " + err);
    }
  });

  autostartCheck.addEventListener("change", async () => {
    try {
      const enabled = await invoke("set_autostart_cmd", { enable: autostartCheck.checked });
      autostartCheck.checked = Boolean(enabled);
      showScheduleFeedback(enabled ? "Desk Pager will start automatically with Windows." : "Start with Windows disabled.");
    } catch (err) {
      showScheduleFeedback("Could not update autostart: " + err);
    }
  });

  testBtn.addEventListener("click", async () => {
    await invoke("send_test_buzz");
    showScheduleFeedback("Test buzz sent — check your phone.");
  });

  stretchBtn.addEventListener("click", async () => {
    await invoke("mark_quick_stretch");
    showScheduleFeedback("Quick stretch reset to 0 — Long Break timer continues.");
    await refreshStatus();
  });

  resetAllBtn.addEventListener("click", async () => {
    await invoke("reset_all_timers");
    showScheduleFeedback("Both Quick Stretch and Long Break timers reset to 0.");
    await refreshStatus();
  });

  meetingBtn.addEventListener("click", async () => {
    const on = await invoke("toggle_meeting");
    showScheduleFeedback(on ? "Meeting hold enabled (auto-expires in 90 min)." : "Meeting hold turned off.");
    await refreshStatus();
  });

  pauseBtn.addEventListener("click", async () => {
    await invoke("toggle_pause");
    await refreshStatus();
  });

  for (const stepBtn of document.querySelectorAll(".step-btn")) {
    stepBtn.addEventListener("click", () => {
      const targetId = stepBtn.getAttribute("data-target");
      const delta = Number(stepBtn.getAttribute("data-delta") || 1);
      const input = document.getElementById(targetId);
      if (!input) return;
      const min = Number(input.min || 1);
      const max = Number(input.max || 360);
      const next = Math.min(max, Math.max(min, (Number(input.value) || min) + delta));
      input.value = next;
      saveScheduleFromForm().catch((err) => showScheduleFeedback("Save failed: " + err));
    });
  }

  for (const group of document.querySelectorAll(".chips")) {
    const targetId = group.getAttribute("data-for");
    const input = document.getElementById(targetId);
    for (const chip of group.querySelectorAll(".chip")) {
      chip.addEventListener("click", () => {
        if (!input) return;
        input.value = chip.getAttribute("data-val");
        saveScheduleFromForm().catch((err) => showScheduleFeedback("Save failed: " + err));
      });
    }
  }

  for (const input of [shortEveryInput, shortDurationInput, longEveryInput, longDurationInput]) {
    input.addEventListener("input", scheduleAutoSave);
    input.addEventListener("change", () => {
      if (saveDebounce) clearTimeout(saveDebounce);
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
