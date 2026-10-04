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
          This daemon needs a token. <code>sci-pi serve</code> prints a link with <code>#token=…</code>, or paste the token here.
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
