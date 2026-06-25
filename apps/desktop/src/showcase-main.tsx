import ReactDOM from "react-dom/client";
import { Showcase } from "./Showcase";
import "./styles/tokens.css";
import "./styles/global.css";
import "./showcase.css";

// Dev-only design-system gallery. Mounts the real `ui/` components against the
// real tokens so the page can never drift from the shipped library. Reach it
// during `pnpm dev` at http://localhost:1420/showcase.html.
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <Showcase />,
);
