/* Preparation-only real GTK3 client, not a compositor or input injector.
 * Build in CI with official libgtk-3-dev:
 * cc -std=c11 -Wall -Wextra -Werror remote-popup-gtk-probe.c \
 *   $(pkg-config --cflags --libs gtk+-3.0) -o remote-popup-gtk-probe
 * Supply an owned WAYLAND_SOCKET fd (or owned WAYLAND_DISPLAY). The harness
 * must independently verify private-seat binding, popup grab and captured pixels.
 * stdout contains only bounded, content-free probe events, never wire logs.
 */
#include <gtk/gtk.h>
#include <gdk/gdkwayland.h>
#include <stdio.h>

static GtkWidget *canvas;
static guint activations;
static guint seats_added;
static int result = 1;

static gboolean draw_canvas(GtkWidget *widget, cairo_t *cr, gpointer data)
{
    (void)widget;
    (void)data;
    if (activations == 0)
        cairo_set_source_rgb(cr, 0.125, 0.25, 0.75);
    else
        cairo_set_source_rgb(cr, 0.125, 0.75, 0.25);
    cairo_paint(cr);
    return TRUE;
}

static void activate_item(GtkMenuItem *item, gpointer data)
{
    (void)item;
    (void)data;
    if (++activations != 1) {
        puts("{\"event\":\"duplicate_activation\"}");
        result = 2;
        gtk_main_quit();
        return;
    }
    /* Only the real toolkit menu activation changes the app's content. */
    gtk_widget_queue_draw(canvas);
    puts("{\"event\":\"menu_activated\",\"count\":1}");
}

static void seat_added(GdkDisplay *display, GdkSeat *seat, gpointer data)
{
    (void)display;
    (void)seat;
    (void)data;
    if (++seats_added <= 8)
        puts("{\"event\":\"toolkit_seat_added\"}");
    /* This is not evidence that GTK selected or bound a wl_pointer for it. */
}

static void mapped(GtkWidget *widget, gpointer data)
{
    (void)widget;
    (void)data;
    puts("{\"event\":\"root_mapped\"}");
}

static gboolean deadline(gpointer data)
{
    (void)data;
    puts("{\"event\":\"probe_deadline\"}");
    /* Application activation alone is not an end-to-end test success. */
    result = activations == 1 ? 0 : 3;
    gtk_main_quit();
    return G_SOURCE_REMOVE;
}

static void closed(GtkWidget *widget, gpointer data)
{
    (void)widget;
    (void)data;
    result = activations == 1 ? 0 : 4;
    gtk_main_quit();
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    g_unsetenv("DISPLAY");
    g_setenv("GDK_BACKEND", "wayland", TRUE);
    gdk_set_allowed_backends("wayland");
    if (!gtk_init_check(&argc, &argv) ||
        !GDK_IS_WAYLAND_DISPLAY(gdk_display_get_default())) {
        fputs("GTK Wayland initialization failed\n", stderr);
        return 5;
    }
    g_signal_connect(gdk_display_get_default(), "seat-added",
                     G_CALLBACK(seat_added), NULL);
    GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_title(GTK_WINDOW(window), "jwm-owned-gtk-popup-probe");
    gtk_window_set_default_size(GTK_WINDOW(window), 320, 240);
    gtk_window_set_resizable(GTK_WINDOW(window), FALSE);
    GtkWidget *box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 0);
    gtk_container_add(GTK_CONTAINER(window), box);
    GtkWidget *button = gtk_menu_button_new();
    gtk_button_set_label(GTK_BUTTON(button), "Open owned menu");
    gtk_widget_set_size_request(button, 320, 48);
    gtk_box_pack_start(GTK_BOX(box), button, FALSE, FALSE, 0);
    GtkWidget *menu = gtk_menu_new();
    GtkWidget *item = gtk_menu_item_new_with_label("Select owned item");
    gtk_menu_shell_append(GTK_MENU_SHELL(menu), item);
    g_signal_connect(item, "activate", G_CALLBACK(activate_item), NULL);
    gtk_widget_show(item);
    gtk_menu_button_set_popup(GTK_MENU_BUTTON(button), menu);
    canvas = gtk_drawing_area_new();
    gtk_widget_set_size_request(canvas, 320, 192);
    g_signal_connect(canvas, "draw", G_CALLBACK(draw_canvas), NULL);
    gtk_box_pack_start(GTK_BOX(box), canvas, TRUE, TRUE, 0);
    g_signal_connect(window, "map", G_CALLBACK(mapped), NULL);
    g_signal_connect(window, "destroy", G_CALLBACK(closed), NULL);
    /* Only show the root widget hierarchy. Never call popup(), show(menu),
     * activate(), synthetic events or input injection. GTK opens the menu in
     * response to the harness's authorized real pointer click. */
    gtk_widget_show_all(window);
    g_timeout_add_seconds(45, deadline, NULL);
    gtk_main();
    return result;
}
