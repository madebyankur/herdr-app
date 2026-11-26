import { run } from "herdr";

// SPIKE: Ctrl-click (Cmd-click) an .html file in a terminal: it opens as a web
// pane beside that terminal.
export function handleLink({ link, source }) {
  if (link.kind !== "file" || !link.path || source.machine || !source.pane) return "pass";
  run("pane.split", { direction: "right", pane: source.pane, web: link.path });
  return "handled";
}
