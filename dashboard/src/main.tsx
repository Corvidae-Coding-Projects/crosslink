import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./index.css";
import { App } from "./App";
import { bootstrapAuth } from "./auth/bootstrap";

// Wire the API client to attach `Authorization: Bearer <token>` before
// React mounts — stores hydrate from the API on the first render, so the
// fetch wrapper must be installed first. See auth/bootstrap.ts for the
// `?token=...` → sessionStorage flow.
bootstrapAuth();

const root = document.getElementById("root");
if (!root) throw new Error("Root element not found");

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
