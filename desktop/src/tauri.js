// Tauri bridge. Resolves the injected `window.__TAURI__` globals once
// (withGlobalTauri:true puts them on `window` before app scripts run) and re-exports
// them. `invoke`/`listen` are detached function references — they don't rely on `this`,
// so calling them standalone (as the app does) is safe.
const TAURI = window.__TAURI__ || null;
export { TAURI };
export const invoke = TAURI && TAURI.core ? TAURI.core.invoke : null;
export const listen = TAURI && TAURI.event ? TAURI.event.listen : null;
export const NATIVE = !!TAURI;
export const appWindow = (TAURI && TAURI.window && TAURI.window.getCurrentWindow) ? TAURI.window.getCurrentWindow() : null;
