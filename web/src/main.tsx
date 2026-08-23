import { StrictMode, lazy } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Route, Routes } from "react-router-dom";
import { Layout } from "./components/Layout";
import { Owners } from "./pages/Owners";
import { Repos } from "./pages/Repos";
import { RepoLayout } from "./pages/RepoLayout";
import { TreePage } from "./pages/TreePage";
import { CommitsPage } from "./pages/CommitsPage";
import { track } from "./data";
import "./styles.css";

// Heavy pages (syntax highlighting / diff rendering / WAL dashboard) are split
// into their own chunks and only downloaded when a user navigates to them.
// `track` shows the chunk download in the top progress bar; the route
// boundaries in Layout/RepoLayout provide the Suspense fallbacks.
const BlobPage = lazy(() => track(import("./pages/BlobPage")).then((m) => ({ default: m.BlobPage })));
const CommitPage = lazy(() => track(import("./pages/CommitPage")).then((m) => ({ default: m.CommitPage })));
const OverviewPage = lazy(() => track(import("./pages/OverviewPage")).then((m) => ({ default: m.OverviewPage })));
const SettingsPage = lazy(() => track(import("./pages/SettingsPage")).then((m) => ({ default: m.SettingsPage })));
const ApiPage = lazy(() => track(import("./pages/ApiPage")).then((m) => ({ default: m.ApiPage })));

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <BrowserRouter>
      <Routes>
        <Route element={<Layout />}>
          <Route index element={<Owners />} />
          <Route path="api" element={<ApiPage />} />
          <Route path=":owner" element={<Repos />} />
          <Route path=":owner/:repo" element={<RepoLayout />}>
            <Route index element={<TreePage />} />
            <Route path="tree/*" element={<TreePage />} />
            <Route path="blob/*" element={<BlobPage />} />
            <Route path="wal" element={<OverviewPage />} />
            <Route path="settings" element={<SettingsPage />} />
            <Route path="commits" element={<CommitsPage />} />
            <Route path="commits/*" element={<CommitsPage />} />
            <Route path="commit/:sha" element={<CommitPage />} />
          </Route>
        </Route>
      </Routes>
    </BrowserRouter>
  </StrictMode>,
);
