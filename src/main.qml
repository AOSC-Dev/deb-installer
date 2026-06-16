import QtQuick 2.15
import QtQuick.Controls 2.15 as Controls
import QtQuick.Layouts 1.15
import QtQuick.Window 2.15
import io.aosc.DebInstaller 1.0
import org.kde.kirigami 2.19 as Kirigami

Kirigami.ApplicationWindow {
    id: root

    property bool view_log: false

    function install_button() {
        switch (installer.action) {
        case DebInstaller.Install:
            return installer.i18n("Install");
        case DebInstaller.Upgrade:
            return installer.i18n("Upgrade");
        case DebInstaller.ReInstall:
            return installer.i18n("Reinstall");
        case DebInstaller.Downgrade:
            return installer.i18n("Downgrade");
        default:
            return installer.i18n("Install");
        }
    }

    // Lock-in width and height - 600x320 for normal views
    width: 600
    minimumWidth: 600
    maximumWidth: 600
    height: 320
    minimumHeight: 320
    maximumHeight: 320
    title: installer.i18n("Package Installation Wizard")
    onClosing: (closeEvent) => {
        if (installer.work_status === DebInstaller.Working)
            closeEvent.accepted = false;
        else
            closeEvent.accepted = true;
    }

    DebInstaller {
        id: installer

        Component.onCompleted: installer.loadPackageInfo()
    }

    pageStack.initialPage: Kirigami.Page {
        id: mainPage

        title: {
            switch (installer.work_status) {
            case DebInstaller.Idle:
                return installer.i18n("Welcome");
            case DebInstaller.Working:
                return installer.i18n("Installing…");
            case DebInstaller.Done:
                if (installer.has_error) {
                    return installer.i18n("Installation Failed!");
                } else {
                    return installer.i18n("Installation Sucessful!");
                }
            default:
                return installer.i18n("Welcome");
            }
        }

        Kirigami.Action {
            id: actionInstall
            text: root.install_button()
            icon.name: "document-save"
            visible: installer.work_status == DebInstaller.Idle &&
                        installer.status === DebInstaller.OkToInstall
            enabled: installer.work_status != DebInstaller.Working
            onTriggered: installer.install()
        }

        Kirigami.Action {
            id: actionViewLog
            text: installer.i18n("View Log")
            visible: installer.work_status == DebInstaller.Done
            icon.name: "text-x-generic"
            onTriggered: {
                root.view_log = !root.view_log;
                if (root.view_log) {
                    // 600x480 for log view
                    mainPage.title = installer.i18n("Installation Log");
                    root.height = 480;
                    root.minimumHeight = 480;
                    root.maximumHeight = 480;
                } else {
                    mainPage.title = installer.i18n("Installation Sucessful!");
                    root.height = 320;
                    root.minimumHeight = 320;
                    root.maximumHeight = 320;
                }
            }
        }

        Kirigami.Action {
            id: actionClose
            text: installer.work_status == DebInstaller.Done ? installer.i18n("Finish") : installer.i18n("Cancel")
            icon.name: "dialog-cancel"
            enabled: installer.work_status != DebInstaller.Working
            onTriggered: Qt.quit()
        }

        Component.onCompleted: {
            var actionList = [actionInstall, actionViewLog, actionClose];
            
            // 探测当前环境：如果 actions 属性支持直接 push，说明是 qt6 kirigami
            if (typeof(mainPage.actions) !== "undefined" && typeof(mainPage.actions.push) === "function") {
                for (var i = 0; i < actionList.length; i++) {
                    mainPage.actions.push(actionList[i]);
                }
            } else {
                // 如果 actions 报错或不可写，说明是老版 Kirigami，我们退回到 contextualActions
                mainPage.contextualActions = actionList;
            }
        }

        // Bottom pane: notice or progress bar.
        ColumnLayout {
            anchors.fill: parent
            anchors.margins: Kirigami.Units.largeSpacing
            spacing: Kirigami.Units.mediumSpacing

            ColumnLayout {
                visible: installer.work_status == DebInstaller.Idle

                Controls.Label {
                    text: installer.i18n("Please note that many 3rd-party software packages are not tested nor verified (they may not function as expected, or may contain malicious content).")
                    wrapMode: Text.WordWrap
                    Layout.fillWidth: true
                }
            }

            ColumnLayout {
                visible: installer.work_status != DebInstaller.Idle && !root.view_log
                
                Controls.ProgressBar {
                    value: installer.progress
                    Layout.fillWidth: true
                    indeterminate: installer.progress === 0
                }
            }
        }

        header: Kirigami.AbstractCard {
            Layout.fillWidth: true
            Layout.preferredHeight: root.view_log ? mainPage.height : Kirigami.Units.gridUnit * 10.5
            implicitHeight: root.view_log ? 
                             ((typeof(mainPage.actions) !== "undefined" && typeof(mainPage.actions.push) === "function") ? 
                               mainPage.height : (mainPage.height - Kirigami.Units.gridUnit * 2.0)) : 
                             Kirigami.Units.gridUnit * 10.5

            // Animation block.
            Rectangle {
                visible: !root.view_log

                width: 166
                height: 166
                color: "transparent"
                opacity: 0.3

                anchors {
                    right: parent.right
                }
                
                Image {
                    id: shakeItem
                    
                    source: "qrc:/qt/qml/io/aosc/DebInstaller/data/io.aosc.deb_installer.svg"

                    width: 128
                    height: 128
                    rotation: 15

                    SequentialAnimation {
                        id: shakeAnimation
                        running: installer.work_status == DebInstaller.Working
                        loops: Animation.Infinite

                        NumberAnimation {
                            target: shakeItem
                            property: "y"
                            to: shakeItem.y - 20
                            duration: 100
                            easing.type: Easing.OutQuad
                        }
                        NumberAnimation {
                            target: shakeItem
                            property: "y"
                            to: shakeItem.y + 20
                            duration: 600
                            easing.type: Easing.OutQuad
                        }
                        NumberAnimation {
                            target: shakeItem
                            property: "y"
                            to: shakeItem.y
                            duration: 50
                            easing.type: Easing.OutQuad
                        }
                    }
                }
            }

            // Wizard banner.
            ColumnLayout {
                visible: !root.view_log
                anchors.fill: parent

                Controls.Label {
                    text: installer.package
                    font.bold: true
                    font.pointSize: 22
                    wrapMode: Text.WordWrap

                    Layout.fillWidth: true
                    Layout.leftMargin: 28
                    // Animation box.
                    Layout.rightMargin: 166
                    Layout.topMargin: 18
                }
                
                Controls.Label {
                    text: installer.pkg_description
                    font.bold: false
                    wrapMode: Text.WordWrap

                    Layout.fillWidth: true
                    Layout.leftMargin: 28
                    // Animation box.
                    Layout.rightMargin: 166
                }

                Controls.Label {
                    visible: installer.work_status == DebInstaller.Idle

                    Layout.leftMargin: 28
                    // Animation box.
                    Layout.rightMargin: 166
                    font.bold: true

                    text: {
                        let status = "";
                        switch (installer.status) {
                        case DebInstaller.OkToInstall:
                            status = installer.i18n("OK to install");
                            break;
                        case DebInstaller.DependencyIssue:
                            status = installer.i18n("Dependencies unmet");
                            break;
                        case DebInstaller.DiskSpaceInsufficient:
                            status = installer.i18n("Insufficient disk space");
                            break;
                        case DebInstaller.Other:
                            status = installer.i18n("Unknown status or error");
                            break;
                        }
                        return status + "・" + installer.pkg_version + "・" + installer.pkg_size;
                    }
                    color: {
                        if (installer.work_status == DebInstaller.OkToInstall) {
                            return "green";
                        } else {
                            return "red";
                        }
                    }
                }

                Controls.Label {
                    visible: installer.work_status != DebInstaller.Idle && !root.view_log

                    text: installer.message
                    font.bold: true
                    wrapMode: Text.WordWrap

                    Layout.fillWidth: true
                    Layout.leftMargin: 28
                    // Animation box.
                    Layout.rightMargin: 166

                    color: {
                        if (installer.has_error) {
                            return "red";
                        } else if (installer.work_status == DebInstaller.Done && !installer.has_error) {
                            return "green";
                        } else {
                            return Kirigami.Theme.textColor;
                        }
                    }
                }

                Item {
                    Layout.fillHeight: true
                }
            }

            background: Rectangle {
                color: root.view_log ? Kirigami.Theme.textColor : Kirigami.Theme.backgroundColor
                radius: 0
            }

            Rectangle {
                visible: root.view_log
                anchors.fill: parent
                id: logWrapper

                color: Kirigami.Theme.textColor
                radius: 0

                ListView {
                    id: logView
                    
                    anchors.fill: parent
                    anchors.margins: Kirigami.Units.largeSpacing
                    spacing: Kirigami.Units.mediumSpacing

                    onCountChanged: {
                        logView.forceLayout();
                        logView.positionViewAtEnd();
                    }
                    onVisibleChanged: {
                        if (visible) {
                            logView.forceLayout();
                        }
                    }
                    Connections {
                        target: installer
                        function onMessageChanged() {
                            logModel.append({ "msg": installer.message });
                        }
                    }

                    model: ListModel {
                        id: logModel
                    }

                    delegate: Controls.Label {
                        text: model.msg
                        font.family: "Monospace"
                        // Equivalent to terminal text colour.
                        color: Kirigami.Theme.backgroundColor;
                        width: logView.width - Kirigami.Units.largeSpacing
                        wrapMode: Text.WordWrap
                    }

                    Controls.ScrollBar.vertical: Controls.ScrollBar {
                        policy: Controls.ScrollBar.AsNeeded
                        height: logView.height
                    }
                }
            }
        }
    }
}
