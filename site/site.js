// Copy buttons for the install commands. Without the Clipboard API the
// buttons stay hidden and the commands remain selectable text.
if (navigator.clipboard) {
  for (const button of document.querySelectorAll("[data-copy]")) {
    const source = document.getElementById(button.dataset.copy);
    const status = document.getElementById(
      button.getAttribute("aria-describedby"),
    );
    if (!source) continue;
    let timer;
    button.hidden = false;
    button.addEventListener("click", async () => {
      clearTimeout(timer);
      try {
        await navigator.clipboard.writeText(source.textContent.trim());
        button.textContent = "Copied";
        if (status) status.textContent = "Copied. Paste it into a terminal.";
      } catch {
        if (status)
          status.textContent =
            "Could not copy. Select the command and copy it by hand.";
      }
      timer = setTimeout(() => {
        button.textContent = "Copy";
        if (status) status.textContent = "";
      }, 4000);
    });
  }
}
