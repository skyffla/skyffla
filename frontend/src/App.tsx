import { useState } from "react";
import shovelImage from "./assets/shovel.png";
import "./App.css";

function GitHubMark() {
  return (
    <svg
      aria-hidden="true"
      viewBox="0 0 24 24"
      className="githubMark"
      focusable="false"
    >
      <path
        fill="currentColor"
        d="M12 .5a12 12 0 0 0-3.79 23.39c.6.11.82-.26.82-.58v-2.03c-3.34.73-4.04-1.42-4.04-1.42-.55-1.38-1.33-1.75-1.33-1.75-1.09-.74.08-.72.08-.72 1.2.09 1.83 1.22 1.83 1.22 1.07 1.81 2.8 1.29 3.49.99.11-.77.42-1.29.76-1.58-2.67-.3-5.47-1.32-5.47-5.87 0-1.3.47-2.36 1.23-3.19-.12-.3-.53-1.52.12-3.17 0 0 1.01-.32 3.3 1.22a11.8 11.8 0 0 1 6 0c2.29-1.54 3.3-1.22 3.3-1.22.65 1.65.24 2.87.12 3.17.77.83 1.23 1.89 1.23 3.19 0 4.57-2.81 5.56-5.49 5.86.43.37.82 1.09.82 2.2v3.27c0 .32.22.7.83.58A12 12 0 0 0 12 .5Z"
      />
    </svg>
  );
}

function CommandBlock({ lines }: { lines: string[] }) {
  return (
    <section className="commandBlock">
      <pre>
        <code>{lines.join("\n")}</code>
      </pre>
    </section>
  );
}

export default function App() {
  const [animateShovel, setAnimateShovel] = useState(true);

  const retriggerShovel = () => {
    setAnimateShovel(false);
    requestAnimationFrame(() => setAnimateShovel(true));
  };

  return (
    <main className="pageShell">
      <div className="ambient ambientLeft" aria-hidden="true" />
      <div className="ambient ambientRight" aria-hidden="true" />

      <section className="heroCard">
        <p className="eyebrow">CLI FIRST P2P FOR THE AGENTIC ERA</p>
        <h1 className="wordmark">skyffla</h1>

        <button
          type="button"
          className={`shovelButton ${animateShovel ? "is-animated" : ""}`}
          onClick={retriggerShovel}
          onAnimationEnd={() => setAnimateShovel(false)}
          aria-label="Replay shovel animation"
        >
          <img src={shovelImage} alt="" className="shovelImage" />
        </button>

        <p className="subtitle">- moving your bits, seamless and secure!</p>

        <p className="lede">
          skyffla.com is now a terminal-native application. Head over to GitHub
          for releases and docs. Browser support will come back later.
        </p>

        <div className="contentGrid">
          <div className="messageCard">
            <h2>Install the CLI</h2>
            <p>Add the tap once, then install.</p>
            <CommandBlock
              lines={["brew tap skyffla/skyffla", "brew install skyffla"]}
            />
          </div>
        </div>

        <div className="actions">
          <a
            className="primaryAction"
            href="https://github.com/skyffla/skyffla"
            target="_blank"
            rel="noreferrer"
          >
            <GitHubMark />
            <span>Open GitHub home</span>
          </a>
        </div>
      </section>
    </main>
  );
}
