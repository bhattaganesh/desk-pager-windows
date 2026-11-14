// Drives the Desk Pager dashboard: Today (live timers), Schedule (settings) and Phone (pairing).

const POLL_MS = 2000;
const SECS_PER_MIN = 60;
const MINS_PER_HOUR = 60;
// Below this, durations read better in seconds ("45 s") than as "1 min".
const SHOW_SECONDS_BELOW = 120;
// Idle time after which a due long break is shown as being taken.
const AWAY_HINT_SECS = 10;
// The label the backend reports as the "call app" while Meeting mode holds reminders.
const MEETING_LABEL = "Meeting mode";
// Gap between clearing and rewriting an action message, so a repeated message is seen and announced again.
const REANNOUNCE_MS = 80;
// Keeps a number and its unit on one line.
const NBSP = "\u00a0";

const byId = (id) => document.getElementById(id);

const appError = byId("app-error");
const versionEl = byId("version-pill");
const installBanner = byId("install-banner");

const stateEl = byId("state");
const stateLabel = byId("state-label");
const stateReason = byId("state-reason");
const shortHeadline = byId("short-headline");
const shortProgress = byId("short-progress");
const shortDetail = byId("short-detail");
const longHeadline = byId("long-headline");
const longProgress = byId("long-progress");
const longDetail = byId("long-detail");
const todayFeedback = byId("today-feedback");
const meetingSwitch = byId("toggle-meeting");
const pauseSwitch = byId("toggle-pause");

const scheduleSummary = byId("schedule-summary");
const scheduleFeedback = byId("schedule-feedback");
const settingsFeedback = byId("settings-feedback");
const autostartSwitch = byId("toggle-autostart");
const notifySwitch = byId("toggle-notify");

const qrEl = byId("qr");
const codeInput = byId("code");
const toggleCodeBtn = byId("toggle-code");
const codeDisclosure = byId("code-disclosure");
const qrColumn = byId("qr-col");
const pairFeedback = byId("pair-feedback");
const testFeedback = byId("test-feedback");
const regenBtn = byId("regenerate");
const regenConfirm = byId("regen-confirm");
const regenFeedback = byId("regen-feedback");

const TABS = ["today", "schedule", "phone"].map((name) => ({
  tab: byId(`tab-${name}`),
  panel: byId(`view-${name}`),
}));
const PHONE_TAB = 2;

// The four schedule numbers: `key` is the settings field, `arg` the save_settings argument.
const FIELDS = [
  { key: "short_every_mins", arg: "shortEveryMins", id: "short-every", unit: "minutes" },
  { key: "short_break_secs", arg: "shortBreakSecs", id: "short-duration", unit: "seconds" },
  { key: "long_every_mins", arg: "longEveryMins", id: "long-every", unit: "minutes" },
  { key: "long_break_mins", arg: "longBreakMins", id: "long-duration", unit: "minutes" },
].map((f) => ({ ...f, input: byId(f.id), msg: byId(`${f.id}-msg`) }));

let invoke = null;
let pollTimer = null;
let savedSettings = null;
let savesInFlight = 0;
let saveChain = Promise.resolve();
const pendingSay = new WeakMap();

// ---------- small DOM helpers: write only when the value changed, so the 2 s poll causes no needless layout ----------

function setText(el, text) {
  if (el.textContent !== text) el.textContent = text;
}

function setAttr(el, name, value) {
  if (el.getAttribute(name) !== value) el.setAttribute(name, value);
}

function setHidden(el, hidden) {
  if (el.hidden !== hidden) el.hidden = hidden;
}

function setProgress(el, done, total, valueText) {
  const ratio = Math.round(Math.max(0, Math.min(1, done / Math.max(1, total))) * 1000) / 1000;
  if (el.value !== ratio) el.value = ratio;
  setAttr(el, "aria-valuetext", valueText);
}

function setSwitch(el, on) {
  setAttr(el, "aria-checked", String(Boolean(on)));
  setText(el.querySelector(".switch-state"), on ? "On" : "Off");
}

function errorText(err) {
  return typeof err === "string" ? err : (err && err.message) || String(err);
}

// Shows the result of an action; errors are worded as errors, not marked by colour alone.
// The text is cleared first so that repeating an action shows, and announces, its result again.
function say(el, message, isError = false) {
  el.classList.toggle("is-error", isError);
  el.textContent = "";
  clearTimeout(pendingSay.get(el));
  const text = isError ? `Error: ${message}` : message;
  pendingSay.set(el, setTimeout(() => (el.textContent = text), REANNOUNCE_MS));
}

// "45 s" under two minutes, otherwise "12 min" or "1 h 20 min". `round` is Math.ceil for time left, Math.floor for time spent.
function formatDuration(secs, round = Math.floor) {
  if (secs < SHOW_SECONDS_BELOW) return `${secs}${NBSP}s`;
  const mins = round(secs / SECS_PER_MIN);
  if (mins < MINS_PER_HOUR) return `${mins}${NBSP}min`;
  const rest = mins % MINS_PER_HOUR;
  const hours = `${Math.floor(mins / MINS_PER_HOUR)}${NBSP}h`;
  return rest === 0 ? hours : `${hours} ${rest}${NBSP}min`;
}

// Runs one command for a control: busy state while it runs, its result or its error shown in `feedbackEl`.
async function runAction(control, feedbackEl, action) {
  if (control.getAttribute("aria-busy") === "true") return;
  control.setAttribute("aria-busy", "true");
  control.setAttribute("aria-disabled", "true");
  try {
    const message = await action();
    if (typeof message === "string") say(feedbackEl, message);
  } catch (err) {
    say(feedbackEl, errorText(err), true);
  } finally {
    control.removeAttribute("aria-busy");
    control.removeAttribute("aria-disabled");
  }
}

// ---------- tabs ----------

function selectTab(index, moveFocus = false) {
  TABS.forEach(({ tab, panel }, i) => {
    const selected = i === index;
    tab.setAttribute("aria-selected", String(selected));
    tab.tabIndex = selected ? 0 : -1;
    panel.hidden = !selected;
  });
  if (index !== PHONE_TAB) hidePairingSecret();
  if (moveFocus) TABS[index].tab.focus();
}

function focusSelectedTab() {
  TABS.find(({ tab }) => tab.getAttribute("aria-selected") === "true").tab.focus();
}

function onTabKeydown(event) {
  const current = TABS.findIndex(({ tab }) => tab === document.activeElement);
  if (current < 0) return;
  const last = TABS.length - 1;
  const target = {
    ArrowRight: current === last ? 0 : current + 1,
    ArrowLeft: current === 0 ? last : current - 1,
    Home: 0,
    End: last,
  }[event.key];
  if (target === undefined) return;
  event.preventDefault();
  selectTab(target, true);
}

// ---------- Today ----------

function setState(kind, label, reason) {
  setAttr(stateEl, "data-kind", kind);
  setText(stateLabel, label);
  setText(stateReason, reason);
}

function renderState(s) {
  if (s.paused) {
    setState("paused", "Paused", "No reminders are sent and both timers stay at zero until you turn Pause off.");
  } else if (s.call_app && s.call_app !== MEETING_LABEL) {
    setState(
      "hold",
      "On hold during a call",
      `${s.call_app} is using the microphone. Sitting time still counts; reminders wait until shortly after the call ends.`
    );
  } else if (s.in_meeting) {
    // `in_meeting` changes with the switch at once; the backend's hold label only follows on its next tick.
    setState("hold", "On hold: Meeting mode is on", "Sitting time still counts. Reminders wait while Meeting mode is on.");
  } else {
    setState("tracking", "Tracking your sitting time", "");
  }
}

function renderShortTimer(s, held) {
  const target = s.break_after_secs;
  const sat = Math.min(s.sitting_secs, target);
  const longDue = s.long_sitting_secs >= s.long_break_after_secs;
  let headline;
  if (s.paused) {
    headline = "Paused";
  } else if (s.stretch_pause_rem_secs > 0) {
    headline = `Stretch now: ${formatDuration(s.stretch_pause_rem_secs, Math.ceil)} left`;
  } else if (sat < target) {
    headline = `Next quick stretch in ${formatDuration(target - sat, Math.ceil)}`;
  } else if (longDue) {
    headline = "Take your long break first";
  } else {
    headline = held ? "Quick stretch due, on hold" : "Quick stretch due now";
  }
  setText(shortHeadline, headline);
  const progressText = `${formatDuration(sat)} of ${formatDuration(target)}`;
  setProgress(shortProgress, sat, target, progressText);
  const done = s.stretches_completed;
  const doneText = done > 0 ? ` · ${done} ${done === 1 ? "stretch" : "stretches"} done since your last long break` : "";
  setText(shortDetail, `Sitting for ${progressText}${doneText}`);
}

function renderLongTimer(s, held) {
  const target = s.long_break_after_secs;
  const sat = Math.min(s.long_sitting_secs, target);
  let headline;
  if (s.paused) {
    headline = "Paused";
  } else if (sat < target) {
    headline = `Long break in ${formatDuration(target - sat, Math.ceil)}`;
  } else if (held) {
    headline = "Long break due, on hold";
  } else if (s.idle_secs >= AWAY_HINT_SECS) {
    const walkSecs = s.long_break_mins * SECS_PER_MIN;
    headline = `On your break: away ${formatDuration(Math.min(s.idle_secs, walkSecs))} of ${s.long_break_mins}${NBSP}min`;
  } else {
    headline = `Long break due now: walk for ${s.long_break_mins}${NBSP}min`;
  }
  setText(longHeadline, headline);
  const progressText = `${formatDuration(sat)} of ${formatDuration(target)}`;
  setProgress(longProgress, sat, target, progressText);
  setText(longDetail, `Sitting for ${progressText} · then a ${s.long_break_mins}${NBSP}min walk`);
}

function renderStatus(s) {
  setText(versionEl, `v${s.version}`);
  setHidden(installBanner, !s.just_installed);
  setSwitch(pauseSwitch, s.paused);
  setSwitch(meetingSwitch, s.in_meeting);
  setSwitch(autostartSwitch, s.autostart);
  setSwitch(notifySwitch, s.desktop_notify);
  renderState(s);
  renderShortTimer(s, s.buzz_held);
  renderLongTimer(s, s.buzz_held);
}

async function refreshStatus() {
  try {
    renderStatus(await invoke("app_status"));
  } catch (err) {
    setState("error", "Can't read status", `${errorText(err)}. Trying again every few seconds.`);
    setText(shortHeadline, "Unknown");
    setText(longHeadline, "Unknown");
  }
}

function startPolling() {
  if (pollTimer !== null || document.hidden) return;
  refreshStatus();
  pollTimer = setInterval(refreshStatus, POLL_MS);
}

function stopPolling() {
  clearInterval(pollTimer);
  pollTimer = null;
}

// ---------- Schedule ----------

function describeSchedule(s) {
  const every = formatDuration(s.short_every_mins * SECS_PER_MIN);
  const after = formatDuration(s.long_every_mins * SECS_PER_MIN);
  return `Quick stretch every ${every} for ${s.short_break_secs}${NBSP}s. Long break after ${after} of sitting, with a ${s.long_break_mins}${NBSP}min walk.`;
}

// Returns why a field's text cannot be saved, or "" when it is a whole number inside its range.
function fieldProblem(field) {
  const raw = field.input.value.trim();
  const value = Number(raw);
  const { min, max } = field.input;
  if (raw === "" || !Number.isInteger(value)) return `Enter a whole number from ${min} to ${max}.`;
  if (value < Number(min) || value > Number(max)) return `Must be from ${min} to ${max} ${field.unit}.`;
  return "";
}

function setFieldMessage(field, message, isError) {
  field.msg.classList.toggle("is-error", isError);
  setText(field.msg, isError && message ? `Error: ${message}` : message);
  if (isError && message) field.input.setAttribute("aria-invalid", "true");
  else field.input.removeAttribute("aria-invalid");
}

// Never rewrite a field under the user's cursor; it is saved again when they commit it.
function setUnlessFocused(input, value) {
  if (document.activeElement !== input) input.value = value;
}

function syncPresetChips() {
  for (const group of document.querySelectorAll(".chips")) {
    const input = byId(group.dataset.for);
    const current = input.value.trim() === "" ? "" : String(Number(input.value));
    for (const chip of group.querySelectorAll(".chip")) {
      setAttr(chip, "aria-pressed", String(chip.dataset.val === current));
    }
  }
}

function applySettings(settings) {
  savedSettings = settings;
  for (const field of FIELDS) setUnlessFocused(field.input, settings[field.key]);
  syncPresetChips();
  setText(scheduleSummary, describeSchedule(settings));
}

function restoreSavedValues() {
  for (const field of FIELDS) {
    field.input.value = savedSettings[field.key];
    setFieldMessage(field, "", false);
  }
  syncPresetChips();
}

// Validates the form after `changed` was committed. Returns what to save, or null when `changed` itself is invalid.
// Other fields holding invalid text keep their saved value.
function buildSaveRequest(changed) {
  const args = {};
  const typed = new Map();
  for (const field of FIELDS) {
    const problem = fieldProblem(field);
    setFieldMessage(field, problem, true);
    if (problem) {
      args[field.arg] = savedSettings[field.key];
    } else {
      args[field.arg] = Number(field.input.value);
      typed.set(field, args[field.arg]);
    }
  }
  return typed.has(changed) ? { args, typed } : null;
}

async function sendSave({ args, typed }) {
  try {
    const saved = await invoke("save_settings", args);
    savedSettings = saved;
    setText(scheduleSummary, describeSchedule(saved));
    // With a newer save already queued, its values (still in the form) must not be overwritten by this result.
    if (savesInFlight === 1) {
      applySettings(saved);
      for (const [field, value] of typed) {
        // The backend keeps a long break from coming sooner than a quick stretch; say so instead of changing it silently.
        if (saved[field.key] !== value) setFieldMessage(field, `Adjusted to ${saved[field.key]} to fit the allowed range.`, false);
      }
    }
    say(scheduleFeedback, "Saved. The new schedule is in force.");
  } catch (err) {
    if (savesInFlight === 1) restoreSavedValues();
    say(scheduleFeedback, `Not saved: ${errorText(err)}. The fields show the schedule still in force.`, true);
  } finally {
    savesInFlight -= 1;
  }
  await refreshStatus();
}

// Saves after `field` was committed. Saves run one at a time, in the order they were committed.
function commitField(field) {
  if (!savedSettings) return say(scheduleFeedback, "the schedule has not loaded yet", true);
  const request = buildSaveRequest(field);
  if (!request) return say(scheduleFeedback, "Not saved. Fix the marked field first.", true);
  savesInFlight += 1;
  saveChain = saveChain.then(() => sendSave(request));
}

// The next value for a stepper: the nearest multiple of the step in that direction (1 -> 5 -> 10), inside the range.
function steppedValue(base, delta, min, max) {
  const step = Math.abs(delta);
  const next = delta > 0 ? Math.floor(base / step) * step + step : Math.ceil(base / step) * step - step;
  return Math.min(max, Math.max(min, next));
}

// Puts `value` in a field; saves it unless it is what is saved already.
function setFieldValue(field, value) {
  const unchanged = savedSettings && value === savedSettings[field.key] && savesInFlight === 0;
  if (unchanged && !fieldProblem(field) && Number(field.input.value) === value) return;
  field.input.value = value;
  syncPresetChips();
  if (unchanged) setFieldMessage(field, "", false);
  else commitField(field);
}

function wireSchedule() {
  for (const field of FIELDS) {
    field.input.addEventListener("input", syncPresetChips);
    // Save only on commit (blur or Enter), never on each keystroke of a half-typed number.
    field.input.addEventListener("change", () => commitField(field));
    // After Enter the field kept its typed text; once focus leaves, show what is really saved.
    field.input.addEventListener("blur", () => {
      if (savedSettings && savesInFlight === 0 && !fieldProblem(field)) {
        field.input.value = savedSettings[field.key];
        syncPresetChips();
      }
    });
  }

  for (const stepBtn of document.querySelectorAll(".step-btn")) {
    const field = FIELDS.find((f) => f.id === stepBtn.dataset.target);
    stepBtn.addEventListener("click", () => {
      if (!savedSettings) return say(scheduleFeedback, "the schedule has not loaded yet", true);
      const base = fieldProblem(field) ? savedSettings[field.key] : Number(field.input.value);
      const { min, max } = field.input;
      setFieldValue(field, steppedValue(base, Number(stepBtn.dataset.delta), Number(min), Number(max)));
    });
  }

  for (const group of document.querySelectorAll(".chips")) {
    const field = FIELDS.find((f) => f.id === group.dataset.for);
    for (const chip of group.querySelectorAll(".chip")) {
      chip.addEventListener("click", () => setFieldValue(field, Number(chip.dataset.val)));
    }
  }
}

// ---------- switches ----------

// Wires a role="switch" button. `change(next)` runs the command and returns the state now in force.
function wireSwitch(control, feedbackEl, change, describe) {
  control.addEventListener("click", () =>
    runAction(control, feedbackEl, async () => {
      const on = await change(control.getAttribute("aria-checked") !== "true");
      setSwitch(control, on);
      await refreshStatus();
      return describe(on);
    })
  );
}

// ---------- Phone ----------

function setCodeVisible(visible) {
  codeInput.type = visible ? "text" : "password";
  toggleCodeBtn.textContent = visible ? "Hide" : "Show";
}

// Puts the typed code back behind dots and closes its disclosure, e.g. when the Phone tab is left.
function hidePairingSecret() {
  setCodeVisible(false);
  codeDisclosure.open = false;
}

function showPairing(info) {
  // The SVG is generated by our own Rust code, never from user or network input.
  qrEl.innerHTML = info.qr_svg;
  codeInput.value = info.code;
  setCodeVisible(false);
  qrColumn.hidden = false;
  codeDisclosure.hidden = false;
}

// Removes a QR code and code that can no longer be trusted, rather than leaving a dead one on screen.
function clearPairing() {
  qrEl.replaceChildren();
  codeInput.value = "";
  hidePairingSecret();
  qrColumn.hidden = true;
  codeDisclosure.hidden = true;
}

// Loads the pairing code that is in force. Returns the pairing info, or throws after clearing the display.
async function loadPairing() {
  try {
    const info = await invoke("pairing_info");
    showPairing(info);
    return info;
  } catch (err) {
    clearPairing();
    throw new Error(`the pairing code cannot be shown (${errorText(err)}). Quit Desk Pager from the tray icon and start it again.`);
  }
}

function closeRegenConfirm() {
  regenConfirm.hidden = true;
  regenBtn.focus();
}

function wirePhone() {
  toggleCodeBtn.addEventListener("click", () => setCodeVisible(codeInput.type === "password"));

  const copyBtn = byId("copy");
  copyBtn.addEventListener("click", () =>
    runAction(copyBtn, pairFeedback, async () => {
      if (!codeInput.value) throw new Error("there is no pairing code to copy");
      try {
        await navigator.clipboard.writeText(codeInput.value);
      } catch (err) {
        throw new Error(`could not copy (${errorText(err)}). Choose Show, select the code and press Ctrl+C.`);
      }
      return "Pairing code copied.";
    })
  );

  const testBtn = byId("test-buzz");
  testBtn.addEventListener("click", () =>
    runAction(testBtn, testFeedback, async () => {
      await invoke("send_test_buzz");
      // Delivery happens in the background, so this is all that is known here.
      return "Test buzz queued — check your phone.";
    })
  );

  regenBtn.addEventListener("click", () => {
    regenConfirm.hidden = false;
    byId("regen-cancel").focus();
  });
  byId("regen-cancel").addEventListener("click", closeRegenConfirm);
  const confirmBtn = byId("regen-confirm-btn");
  confirmBtn.addEventListener("click", () =>
    runAction(confirmBtn, regenFeedback, async () => {
      closeRegenConfirm();
      try {
        showPairing(await invoke("regenerate_pairing"));
      } catch (err) {
        // The old QR code may be dead now: show whatever code is really in force, or nothing.
        await loadPairing();
        throw new Error(`${errorText(err)}. The QR code above is the one in force now; pair your phone with it again.`);
      }
      return "New code ready. Pair your phone again by scanning the new QR code.";
    })
  );
}

// ---------- start-up ----------

function wireToday() {
  const stretchBtn = byId("mark-stretch");
  stretchBtn.addEventListener("click", () =>
    runAction(stretchBtn, todayFeedback, async () => {
      const reset = await invoke("mark_quick_stretch");
      await refreshStatus();
      return reset === "both"
        ? "Both timers restarted from zero, because you had been sitting for less than one quick-stretch interval."
        : "Quick-stretch timer restarted. The long-break timer keeps counting.";
    })
  );

  const resetBtn = byId("reset-all");
  resetBtn.addEventListener("click", () =>
    runAction(resetBtn, todayFeedback, async () => {
      await invoke("reset_all_timers");
      await refreshStatus();
      return "Both timers restarted from zero.";
    })
  );

  wireSwitch(
    meetingSwitch,
    todayFeedback,
    () => invoke("toggle_meeting"),
    (on) => (on ? "Meeting mode is on. Reminders are held." : "Meeting mode is off.")
  );
  wireSwitch(
    pauseSwitch,
    todayFeedback,
    () => invoke("toggle_pause"),
    (on) => (on ? "Reminders paused." : "Reminders resumed. Both timers start from zero.")
  );
}

function wireSettings() {
  wireSwitch(
    autostartSwitch,
    settingsFeedback,
    (next) => invoke("set_autostart_cmd", { enable: next }),
    (on) => (on ? "Desk Pager will start with Windows." : "Desk Pager will not start with Windows.")
  );
  wireSwitch(
    notifySwitch,
    settingsFeedback,
    (next) => invoke("set_desktop_notify_cmd", { enable: next }),
    (on) => (on ? "This PC will also show a silent notification with each buzz." : "Buzzes go to your phone only.")
  );
}

function showAppError(message) {
  appError.hidden = false;
  setText(appError, `Error: ${message}`);
}

async function init() {
  if (!window.__TAURI__) {
    showAppError("Desk Pager's window could not connect to the app. Close this window and open it again from the tray icon.");
    return;
  }
  invoke = window.__TAURI__.core.invoke;

  byId("tab-today").parentElement.addEventListener("keydown", onTabKeydown);
  TABS.forEach(({ tab }, index) => tab.addEventListener("click", () => selectTab(index)));
  const dismissBtn = byId("dismiss-install");
  dismissBtn.addEventListener("click", () =>
    runAction(dismissBtn, todayFeedback, async () => {
      await invoke("dismiss_install_banner");
      installBanner.hidden = true;
      focusSelectedTab();
    })
  );
  wireToday();
  wireSchedule();
  wireSettings();
  wirePhone();

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      stopPolling();
      hidePairingSecret();
    } else {
      startPolling();
    }
  });
  startPolling();

  try {
    const info = await loadPairing();
    if (info.first_run) selectTab(PHONE_TAB);
  } catch (err) {
    say(pairFeedback, errorText(err), true);
    showAppError(errorText(err));
  }

  try {
    applySettings(await invoke("get_settings"));
  } catch (err) {
    say(scheduleFeedback, `could not load the schedule: ${errorText(err)}`, true);
  }

  // Diagnostics only (a log line in the app): its failure must not look like a broken window.
  invoke("pairing_rendered", { qrShown: qrEl.querySelector("svg") !== null }).catch((err) =>
    console.warn("pairing_rendered failed:", errorText(err))
  );
}

init().catch((err) => showAppError(`Desk Pager's window did not start properly: ${errorText(err)}`));
