// Real Qt Widgets client. No synthetic events, menu popup(), or action trigger().
#include <QApplication>
#include <QAction>
#include <QMenu>
#include <QPainter>
#include <QTimer>
#include <QToolButton>
#include <QWidget>
#include <cstdio>

class Canvas final : public QWidget {
public:
    bool activated = false;
    using QWidget::QWidget;
protected:
    void paintEvent(QPaintEvent *) override {
        QPainter painter(this);
        painter.fillRect(rect(), activated ? QColor(32, 192, 64) : QColor(32, 64, 192));
    }
};

int main(int argc, char **argv) {
    std::setvbuf(stdout, nullptr, _IOLBF, 0);
    qunsetenv("DISPLAY");
    qputenv("QT_QPA_PLATFORM", "wayland");
    qputenv("QT_WAYLAND_DISABLE_WINDOWDECORATION", "1");
    QApplication app(argc, argv);
    if (QGuiApplication::platformName() != QStringLiteral("wayland")) return 5;
    QApplication::setStyle(QStringLiteral("Fusion"));
    QWidget root;
    root.setWindowTitle(QStringLiteral("owned-qt-popup-probe"));
    root.setFixedSize(320, 240);
    auto *canvas = new Canvas(&root);
    canvas->setGeometry(0, 48, 320, 192);
    auto *button = new QToolButton(&root);
    button->setGeometry(0, 0, 320, 48);
    button->setText(QStringLiteral("Open owned menu"));
    button->setPopupMode(QToolButton::InstantPopup);
    auto *menu = new QMenu(button);
    menu->setStyleSheet(QStringLiteral("QMenu { background: #e0d040; margin: 0px; border: 1px solid black; } QMenu::item { padding: 10px 24px; }"));
    auto *item = menu->addAction(QStringLiteral("Select owned item"));
    button->setMenu(menu);
    unsigned activations = 0;
    QObject::connect(item, &QAction::triggered, &app, [&] {
        if (++activations != 1) {
            std::puts("QT_PROBE_DUPLICATE");
            app.exit(2);
            return;
        }
        canvas->activated = true;
        canvas->update();
        std::puts("QT_PROBE_ACTIVATED_ONCE");
    });
    QTimer::singleShot(45000, &app, [&] { app.exit(3); });
    root.show(); // Only the root; Qt must open its menu from real pointer events.
    return app.exec();
}
