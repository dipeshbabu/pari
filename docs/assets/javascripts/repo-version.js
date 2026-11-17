// Show the newest release tag in the header repository widget.
//
// The theme reads GitHub's "latest release", which excludes pre-releases.
// Pari's 0.x releases are published as pre-releases, so fall back to the
// newest release of any kind and add it beside the star and fork counts.
(function () {
  "use strict";

  var API = "https://api.github.com/repos/dipeshbabu/pari/releases?per_page=1";
  var CACHE_KEY = "pari:repo-version";
  var pending = null;

  function readCache() {
    try {
      return window.sessionStorage.getItem(CACHE_KEY);
    } catch (error) {
      return null;
    }
  }

  function writeCache(value) {
    try {
      window.sessionStorage.setItem(CACHE_KEY, value);
    } catch (error) {
      // Storage can be unavailable; the version is simply fetched again.
    }
  }

  function latestTag() {
    var cached = readCache();
    if (cached) {
      return Promise.resolve(cached);
    }
    if (!pending) {
      pending = fetch(API, { headers: { Accept: "application/vnd.github+json" } })
        .then(function (response) {
          return response.ok ? response.json() : [];
        })
        .then(function (releases) {
          var tag = releases.length ? releases[0].tag_name : "";
          if (tag) {
            writeCache(tag);
          }
          return tag;
        })
        .catch(function () {
          return "";
        });
    }
    return pending;
  }

  function addVersion() {
    var facts = document.querySelectorAll(".md-source__facts");
    if (!facts.length) {
      return;
    }
    latestTag().then(function (tag) {
      if (!tag) {
        return;
      }
      facts.forEach(function (list) {
        if (list.querySelector(".md-source__fact--version")) {
          return;
        }
        var item = document.createElement("li");
        item.className = "md-source__fact md-source__fact--version";
        item.textContent = tag;
        list.insertBefore(item, list.firstChild);
      });
    });
  }

  // The theme renders the facts asynchronously, so watch for them.
  new MutationObserver(addVersion).observe(document.body, {
    childList: true,
    subtree: true,
  });
  addVersion();
})();
