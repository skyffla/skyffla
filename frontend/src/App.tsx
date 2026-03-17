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

function XMark() {
  return (
    <svg
      aria-hidden="true"
      viewBox="0 0 24 24"
      className="xMark"
      focusable="false"
    >
      <path
        fill="currentColor"
        d="M18.9 2H22l-6.77 7.74L23 22h-6.09l-4.77-7.4L5.67 22H2.56l7.24-8.27L1 2h6.24l4.31 6.82L18.9 2Zm-1.07 18h1.69L6.32 3.89H4.5L17.83 20Z"
      />
    </svg>
  );
}

function CopyIcon({ copied }: { copied: boolean }) {
  if (copied) {
    return (
      <svg
        aria-hidden="true"
        viewBox="0 0 20 20"
        className="copyIcon"
        focusable="false"
      >
        <path
          fill="currentColor"
          d="M8.25 13.55 4.7 10l-1.4 1.4 4.95 4.95L16.7 7.9l-1.4-1.4-7.05 7.05Z"
        />
      </svg>
    );
  }

  return (
    <svg
      aria-hidden="true"
      viewBox="0 0 20 20"
      className="copyIcon"
      focusable="false"
    >
      <path
        fill="currentColor"
        d="M6 2.5A1.5 1.5 0 0 0 4.5 4v9A1.5 1.5 0 0 0 6 14.5h7A1.5 1.5 0 0 0 14.5 13V4A1.5 1.5 0 0 0 13 2.5H6Zm0 1.5h7v9H6V4Zm-3 3A1.5 1.5 0 0 0 1.5 8.5v8A1.5 1.5 0 0 0 3 18h7.5v-1.5H3v-8h-1.5Z"
      />
    </svg>
  );
}

function CommandBlock({ lines }: { lines: string[] }) {
  const [copiedLine, setCopiedLine] = useState<string | null>(null);

  const copyLine = async (line: string) => {
    try {
      await navigator.clipboard.writeText(line);
      setCopiedLine(line);
      window.setTimeout(() => {
        setCopiedLine((current) => (current === line ? null : current));
      }, 1400);
    } catch {
      setCopiedLine(null);
    }
  };

  return (
    <section className="commandBlock">
      {lines.map((line) => (
        <div key={line} className="commandLine">
          <div className="commandText">
            <span className="commandPrompt" aria-hidden="true">
              $
            </span>
            <code>{line}</code>
          </div>
          <button
            type="button"
            className={`copyButton ${copiedLine === line ? "is-copied" : ""}`}
            onClick={() => void copyLine(line)}
            aria-label={`Copy command: ${line}`}
            title={copiedLine === line ? "Copied" : "Copy command"}
          >
            <CopyIcon copied={copiedLine === line} />
          </button>
        </div>
      ))}
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
        <p className="productStatement">
          multi-party peer-to-peer rooms for terminals, agents, and scripts
        </p>

        <ul className="lede" aria-label="intro notes">
          <li>skyffla is now a terminal-native application written in rust</li>
          <li>
            <a
              className="ledeLink"
              href="https://pypi.org/project/skyffla/"
              target="_blank"
              rel="noreferrer"
            >
              python
            </a>{" "}
            and{" "}
            <a
              className="ledeLink"
              href="https://www.npmjs.com/package/skyffla"
              target="_blank"
              rel="noreferrer"
            >
              npm
            </a>{" "}
            wrappers plug skyffla straight into your agents
          </li>
          <li>
            see examples on{" "}
            <a
              className="ledeLink"
              href="https://github.com/skyffla/skyffla"
              target="_blank"
              rel="noreferrer"
            >
              GitHub
            </a>{" "}
            and{" "}
            <a
              className="ledeLink"
              href="https://x.com/skyffla"
              target="_blank"
              rel="noreferrer"
            >
              follow on X
            </a>{" "}
            to come along for the ride
          </li>
        </ul>

        <div className="contentGrid">
          <section className="messageCard installCard">
            <div className="installColumns">
              <div className="installColumn">
                <section className="installGroup">
                  <div className="installHeader">
                    <h2 className="sectionLabel">install cli</h2>
                  </div>
                  <CommandBlock
                    lines={[
                      "brew tap skyffla/skyffla",
                      "brew install skyffla",
                      "skyffla help",
                    ]}
                  />
                </section>

                <section className="installGroup">
                  <p className="sectionLabel installHint">Need Homebrew? macOS/Linux</p>
                  <CommandBlock
                    lines={[
                      '/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"',
                    ]}
                  />
                </section>
              </div>

              <div className="installColumn">
                <section className="installGroup">
                  <div className="installHeader">
                    <h2 className="sectionLabel">python wrapper</h2>
                  </div>
                  <CommandBlock lines={["pip install skyffla"]} />
                </section>

                <section className="installGroup">
                  <p className="sectionLabel installHint">Node.js wrapper</p>
                  <CommandBlock lines={["npm install skyffla"]} />
                </section>
              </div>
            </div>
          </section>
        </div>

        <div className="actions bottomActions">
          <a
            className="primaryAction brandAction githubAction"
            href="https://github.com/skyffla/skyffla"
            target="_blank"
            rel="noreferrer"
            aria-label="Open on GitHub"
            title="Open on GitHub"
          >
            <GitHubMark />
          </a>
          <a
            className="primaryAction brandAction xAction"
            href="https://x.com/skyffla"
            target="_blank"
            rel="noreferrer"
            aria-label="Follow on X"
            title="Follow on X"
          >
            <XMark />
          </a>
          <a
            className="primaryAction brandAction pypiAction"
            href="https://pypi.org/project/skyffla/"
            target="_blank"
            rel="noreferrer"
            aria-label="Open on PyPI"
            title="Open on PyPI"
          >
            <span className="brandBadge pypiBadge" aria-hidden="true">
              PyPI
            </span>
          </a>
          <a
            className="primaryAction brandAction npmAction"
            href="https://www.npmjs.com/package/skyffla"
            target="_blank"
            rel="noreferrer"
            aria-label="Open on npm"
            title="Open on npm"
          >
            <span className="brandBadge npmBadge" aria-hidden="true">
              npm
            </span>
          </a>
        </div>
      </section>
    </main>
  );
}
