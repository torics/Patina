import { render } from "preact";
import { App } from "./app";
import { applyTheme, boot, theme } from "./state";
import "./style.css";

applyTheme(theme.value);
render(<App />, document.getElementById("app")!);
boot();
