import { useEffect, useState } from "react";
import { Api, type AuthIdentity, type DeviceCredential } from "../api";
import { forgetToken, store, type HostState } from "../store";
import { Modal } from "./Modal";
import "./DeviceSettings.css";

export function DeviceSettings({ h, onClose }: { h: HostState; onClose: () => void }) {
  const [identity, setIdentity] = useState<AuthIdentity | null>(null);
  const [adminApi, setAdminApi] = useState<Api | null>(null);
  const [devices, setDevices] = useState<DeviceCredential[]>([]);
  const [master, setMaster] = useState("");
  const [name, setName] = useState("Browser");
  const [pair, setPair] = useState<{ url: string; expires_at: number } | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let gone = false;
    void h.api.authMe().then(async (who) => {
      if (gone) return;
      setIdentity(who);
      if (who.admin) {
        const list = await h.api.devices();
        if (!gone) { setAdminApi(h.api); setDevices(list); }
      }
    }).catch((e: Error) => { if (!gone) setErr(e.message); });
    return () => { gone = true; };
  }, [h.api]);

  const manage = async () => {
    setBusy(true); setErr(null);
    try {
      const api = new Api({ url: h.api.url, token: master.trim() });
      const who = await api.authMe();
      if (!who.admin) throw new Error("An administrator credential is required.");
      const list = await api.devices();
      setAdminApi(api); setDevices(list); setMaster("");
    } catch (e) { setErr((e as Error).message); }
    finally { setBusy(false); }
  };

  const revoke = async (device: DeviceCredential) => {
    if (!adminApi || !window.confirm(`Revoke ${device.name}? Its active connections will close immediately.`)) return;
    setBusy(true); setErr(null);
    try {
      await adminApi.revokeDevice(device.id);
      if (device.id === identity?.device_id && store.mode === "direct") { forgetToken(); onClose(); }
      else setDevices(await adminApi.devices());
    } catch (e) { setErr((e as Error).message); }
    finally { setBusy(false); }
  };

  const pairBrowser = async () => {
    if (!adminApi) return;
    setBusy(true); setErr(null); setPair(null);
    try { setPair(await adminApi.issuePair(name.trim())); }
    catch (e) { setErr((e as Error).message); }
    finally { setBusy(false); }
  };

  const revokeOwn = async () => {
    if (!window.confirm("Revoke this browser's credential and close all its connections?")) return;
    setBusy(true); setErr(null);
    try {
      await h.api.revokeSelf();
      if (store.mode === "direct") forgetToken();
      onClose();
    } catch (e) { setErr((e as Error).message); }
    finally { setBusy(false); }
  };

  return (
    <Modal title={`Devices · ${h.name}`} onClose={onClose}>
      <div className="device-settings">
        <p className="dim">Browser credentials are stored separately and can be revoked without changing the CLI/admin token or other devices.</p>
        <p className="dim">Only pair trusted devices. Sessions and terminals can run commands as this daemon's Unix account. Revoking a bearer closes its connections; it does not undo commands, installed access, or credentials already copied.</p>
        {identity && <section>
          <h3>This connection</h3>
          <p>{identity.admin ? "Administrator token" : identity.device_id ? "Paired browser" : "Allowed Tailscale identity"}</p>
          {identity.device_id && <button className="btn" disabled={busy} onClick={() => void revokeOwn()}>Revoke this browser &amp; log out</button>}
          {store.mode === "direct" && <button className="btn" disabled={busy} onClick={() => { forgetToken(); onClose(); }}>Log out locally</button>}
        </section>}
        <section>
          <h3>Administrator management</h3>
          {!adminApi ? (
            <form className="form" onSubmit={(e) => { e.preventDefault(); void manage(); }}>
              <p className="dim">Use <code>sci-pi token</code> locally. This management credential stays only in memory until this dialog closes.</p>
              <input type="password" aria-label="Administrator token" autoComplete="off" placeholder="Administrator token" value={master} onChange={(e) => setMaster(e.target.value)} />
              <button className="btn" disabled={busy || !master.trim()}>Manage devices</button>
            </form>
          ) : <>
            <button className="btn" disabled={busy} onClick={async () => {
              setBusy(true); setErr(null);
              try { setDevices(await adminApi.devices()); }
              catch (e) { setErr((e as Error).message); }
              finally { setBusy(false); }
            }}>Refresh devices</button>
            <ul className="device-list">
              {devices.map((device) => <li key={device.id}>
                <div><strong>{device.name}</strong>{device.id === identity?.device_id && <span className="dim"> · this browser</span>}<div className="dim">Paired {new Date(device.created_at * 1000).toLocaleString()}</div></div>
                <button className="btn" disabled={busy} onClick={() => void revoke(device)}>Revoke</button>
              </li>)}
            </ul>
            {!devices.length && <p className="dim">No paired devices.</p>}
            <form className="form" onSubmit={(e) => { e.preventDefault(); void pairBrowser(); }}>
              <label className="field"><span className="label">New device name</span><input value={name} maxLength={100} onChange={(e) => setName(e.target.value)} /></label>
              <button className="btn" disabled={busy}>Issue local pairing link</button>
              <p className="dim">Pairing must be issued and opened on this daemon's loopback address. For another local browser, run <code>sci-pi pair --name "Laptop"</code>. Remote and Tailscale origins cannot redeem a pairing.</p>
              {pair && <div className="pair-result"><p>One use. Expires {new Date(pair.expires_at * 1000).toLocaleTimeString()}.</p><input aria-label="Short-lived pairing link" readOnly value={pair.url} onFocus={(e) => e.target.select()} /></div>}
            </form>
          </>}
        </section>
        {err && <div className="form-error" role="alert">{err}</div>}
      </div>
    </Modal>
  );
}
