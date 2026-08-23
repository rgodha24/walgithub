import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

const plugins = [remarkGfm];

export default function MarkdownRenderer({ source }: { source: string }) {
  return <ReactMarkdown remarkPlugins={plugins}>{source}</ReactMarkdown>;
}
