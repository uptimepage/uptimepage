// The status region refreshes by swapping its markup, which would close every
// row a visitor opened. Rows marked data-keep-open are reopened by id.
(function () {
    "use strict";

    var open = [];

    document.addEventListener("htmx:beforeSwap", function () {
        var nodes = document.querySelectorAll("details[data-keep-open][open]");
        open = Array.prototype.map.call(nodes, function (d) { return d.id; });
    });

    document.addEventListener("htmx:afterSwap", function () {
        for (var i = 0; i < open.length; i++) {
            var d = open[i] && document.getElementById(open[i]);
            if (d) d.open = true;
        }
    });
})();
