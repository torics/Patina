import { render } from "preact";
import { App } from "./app";
import { boot } from "./state";
import "./style.css";

render(<App />, document.getElementById("app")!);
boot();
