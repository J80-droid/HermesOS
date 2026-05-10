import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import App from "./App";
import { Boot } from "./components/Boot";
import { SystemActionsProvider } from "./contexts/SystemActions";
import { I18nProvider } from "./i18n/context";
import { ThemeProvider } from "./themes/context";
import "./index.css";
import "./styles.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <BrowserRouter>
      <ThemeProvider>
        <I18nProvider>
          <SystemActionsProvider>
            <Boot>
              <App />
            </Boot>
          </SystemActionsProvider>
        </I18nProvider>
      </ThemeProvider>
    </BrowserRouter>
  </StrictMode>,
);
