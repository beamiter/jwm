// Native WebKitGTK regression for https://bugs.webkit.org/show_bug.cgi?id=319864
// Build with GTK3 and WebKitGTK 4.1 pkg-config flags; see vendor/webkitgtk/README.md.
// Run under Xvfb (or DISPLAY on a desktop). A failing library exits 1.
// Compare native backing pixels after repeated partial updates to a freshly
// rendered native view. A rectangular translucent background isolates copy
// semantics from antialiased rounded-clip rasterization between repaints.
#include <gtk/gtk.h>
#include <webkit2/webkit2.h>
#include <stdio.h>
#include <stdlib.h>

static WebKitWebView *view;
static WebKitWebView *reference_view;
static GtkWidget *reference_window;
static unsigned tick;
static int result = 2;

static const char *html =
    "<html><style>html,body{margin:0;background:transparent}"
    ".bar{background:rgba(246,246,248,.58);height:42px;"
    "display:flex;align-items:center;justify-content:end;font:14px sans-serif}"
    ".metric{padding:0 15px}</style><div class=bar>"
    "<div class=metric id=metric>NET 1 KiB/s</div></div></html>";

static WebKitWebView *create_view(int y, GtkWidget **window)
{
    *window = gtk_window_new(GTK_WINDOW_POPUP);
    gtk_window_set_default_size(GTK_WINDOW(*window), 1000, 42);
    gtk_window_move(GTK_WINDOW(*window), 0, y);
    gtk_widget_set_visual(*window, gdk_screen_get_rgba_visual(gtk_widget_get_screen(*window)));
    gtk_widget_set_app_paintable(*window, TRUE);
    WebKitWebView *web_view = WEBKIT_WEB_VIEW(webkit_web_view_new());
    // Exercise the persistent CPU backing store independent of GPU availability.
    webkit_settings_set_hardware_acceleration_policy(webkit_web_view_get_settings(web_view),
        WEBKIT_HARDWARE_ACCELERATION_POLICY_NEVER);
    GdkRGBA transparent = {0, 0, 0, 0};
    webkit_web_view_set_background_color(web_view, &transparent);
    gtk_container_add(GTK_CONTAINER(*window), GTK_WIDGET(web_view));
    gtk_widget_show_all(*window);
    return web_view;
}

static gboolean compare_views(gpointer unused)
{
    GdkPixbuf *expected = gdk_pixbuf_get_from_window(
        gtk_widget_get_window(GTK_WIDGET(reference_view)), 0, 0, 1000, 42);
    GdkPixbuf *actual = gdk_pixbuf_get_from_window(
        gtk_widget_get_window(GTK_WIDGET(view)), 0, 0, 1000, 42);
    if (expected && actual) {
        unsigned bad = 0;
        int ac = gdk_pixbuf_get_n_channels(actual);
        int bc = gdk_pixbuf_get_n_channels(expected);
        if (ac != 4 || bc != 4) {
            g_printerr("The test requires native RGBA windows\n");
            g_clear_object(&expected);
            g_clear_object(&actual);
            gtk_main_quit();
            return G_SOURCE_REMOVE;
        }
        for (int y = 0; y < 42; ++y) {
            const guchar *a = gdk_pixbuf_get_pixels(actual) + y * gdk_pixbuf_get_rowstride(actual);
            const guchar *b = gdk_pixbuf_get_pixels(expected) + y * gdk_pixbuf_get_rowstride(expected);
            for (int x = 0; x < 1000; ++x) {
                int aa = a[x * ac + 3];
                int ba = b[x * bc + 3];
                gboolean different = abs(aa - ba) > 2;
                // RGB at zero alpha is undefined. Compare premultiplied color.
                for (int c = 0; c < 3; ++c)
                    different |= abs(a[x * ac + c] * aa / 255 - b[x * bc + c] * ba / 255) > 2;
                bad += different;
            }
        }
        unsigned alpha = gdk_pixbuf_get_pixels(actual)[20 * gdk_pixbuf_get_rowstride(actual) + 500 * 4 + 3];
        g_print("WebKitGTK %u.%u.%u: %u differing pixels after 12 updates; background alpha=%u (expected 148)\n",
            webkit_get_major_version(), webkit_get_minor_version(), webkit_get_micro_version(), bad, alpha);
        result = bad || alpha != 148 ? 1 : 0;
    }
    g_clear_object(&expected);
    g_clear_object(&actual);
    gtk_main_quit();
    return G_SOURCE_REMOVE;
}

static void reference_loaded(WebKitWebView *unused, WebKitLoadEvent event, gpointer data)
{
    if (event == WEBKIT_LOAD_FINISHED)
        g_timeout_add(500, compare_views, NULL);
}

static gboolean step(gpointer unused)
{
    if (++tick == 13) {
        // One interval after the last update lets the native frame settle.
        reference_view = create_view(64, &reference_window);
        g_signal_connect(reference_view, "load-changed", G_CALLBACK(reference_loaded), NULL);
        webkit_web_view_load_html(reference_view, html, NULL);
        return G_SOURCE_REMOVE;
    }
    char js[160];
    snprintf(js, sizeof js, "document.getElementById('metric').textContent='NET %u KiB/s'", tick % 2 ? 888 : 1);
    webkit_web_view_evaluate_javascript(view, js, -1, NULL, NULL, NULL, NULL, NULL);
    return G_SOURCE_CONTINUE;
}

static void loaded(WebKitWebView *unused, WebKitLoadEvent event, gpointer data)
{
    if (event == WEBKIT_LOAD_FINISHED)
        g_timeout_add(300, step, NULL);
}

static gboolean timeout(gpointer unused)
{
    g_printerr("Timed out waiting for WebKit\n");
    gtk_main_quit();
    return G_SOURCE_REMOVE;
}

int main(int argc, char **argv)
{
    gtk_init(&argc, &argv);
    GtkWidget *window;
    view = create_view(0, &window);
    g_signal_connect(view, "load-changed", G_CALLBACK(loaded), NULL);
    webkit_web_view_load_html(view, html, NULL);
    g_timeout_add_seconds(30, timeout, NULL);
    gtk_main();
    gtk_widget_destroy(window);
    if (reference_window)
        gtk_widget_destroy(reference_window);
    return result;
}
