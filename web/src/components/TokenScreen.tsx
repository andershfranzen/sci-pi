import { useState } from "react";
import { setToken, store } from "../store";
import { LogoMark } from "./Icons";

export function TokenScreen() {
  const [value, setValue] = useState("");
  return (
    <div className="token-screen">
      <form
        className="token-card"
        onSubmit={(e) => {
          e.preventDefault();
          setToken(value);
        }}
      >
        <div className="token-brand">
          <LogoMark size={28} />
          <span>sci-pi</span>
        </div>
        <p className="dim">
          Run <code>sci-pi pair</code> on this machine to connect this browser with a revocable device credential. You can also paste a CLI/admin token below.
        </p>
        <input
          type="password"
          className="mono"
          autoFocus
          placeholder="Token"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          autoComplete="current-password"
          aria-label="Token"
        />
        {store.tokenError && <div className="form-error">{store.tokenError}</div>}
        <button className="btn btn-primary" type="submit" disabled={!value.trim()}>
          Connect
        </button>
      </form>
    </div>
  );
}
