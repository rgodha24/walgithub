import { Suspense, lazy } from "react";

// react-markdown + remark-gfm (micromark, mdast, hast…) is ~100 kB gzipped;
// only pay for it when a README/markdown blob is actually rendered.
const Renderer = lazy(() => import("./MarkdownRenderer"));

export function Markdown({ source }: { source: string }) {
  return (
    <div className="markdown-body">
      <Suspense fallback={<pre className="code-block">{source}</pre>}>
        <Renderer source={source} />
      </Suspense>
    </div>
  );
}
